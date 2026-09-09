use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use caudra_config::{FILE_WRITE_TOOLS, ToolKey};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::command_pattern::{PatternFault, grade_command_pattern};
use crate::tools::PermissionIntent;

pub use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionExecutorKind,
    PermissionLifetime, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
    SelectedPermissionArgument, StructuredPermissionEffect, StructuredPermissionRule,
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
const SUMMARY_MAX_CHARS: usize = 240;
const LISTED_COMMANDS_MAX: usize = 3;
const REVIEW_MAX_DEPTH: usize = 6;
const REVIEW_MAX_ITEMS: usize = 32;
const FILE_READ_TOOLS: &[&str] = &["file_read", "file_index", "read", "view_image"];
const DIRECTORY_READ_TOOLS: &[&str] = &["list"];
const FILE_SEARCH_TOOLS: &[&str] = &["file_glob", "file_grep", "glob", "grep"];
const GIT_METADATA_DIR: &str = ".git";
const SUBTREE_OPTION_ID: &str = "allow_filesystem_subtree";
const OUTSIDE_HOME_PHRASE: &str = "ALLOW OUTSIDE HOME";
pub(super) const BROAD_SHELL_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";
const WORKDIR_ATTRIBUTE: &str = "workdir";
/// The executable-name-resolved form of a command, set by the shell tool.
/// Restrictive policy is matched against it as well as the reviewed text, so a
/// deny cannot be dodged by spelling the executable as a path.
pub(super) const NORMALIZED_COMMAND_ATTRIBUTE: &str = "normalized_command";
pub const COMMAND_GROUP_PREFIX: &str = "command_";
pub const COMMAND_EXACT_PREFIX: &str = "command_exact_";
pub const COMMAND_PATTERN_PREFIX: &str = "command_pattern_";
const EXACT_COMMAND_CHIP: &str = "this command";
const EXACT_COMMANDS_OPTION: &str = "allow_exact_commands";
const COMMAND_PATTERNS_OPTION: &str = "allow_command_patterns";
/// The whole-request shell authorities a per-command answer reproduces exactly:
/// every row on its narrowest reusable rung is one, every row at its widest is
/// the other. A prompt that offers the rows has no reason to offer these too.
pub const COMPOSABLE_SHELL_OPTIONS: &[&str] = &[EXACT_COMMANDS_OPTION, COMMAND_PATTERNS_OPTION];
const INERT_GIT_METADATA: &[&str] = &[
    "COMMIT_EDITMSG",
    "FETCH_HEAD",
    "HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "ORIG_HEAD",
    "file_index",
    "logs",
    "objects",
    "packed-refs",
    "refs",
];

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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
#[serde(rename_all = "snake_case")]
pub enum PermissionRowGrant {
    /// A rung the request offered for this resource.
    Offered(String),
    /// A pattern the user wrote for this resource.
    Written(String),
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

    /// How long coverage from this origin can be relied on, for deciding
    /// whether granting again would add anything.
    ///
    /// The builtin allowlist has no answer: it is a default consulted only
    /// where no rule speaks, so a grant over it is a real grant. A plugin's
    /// rules last only as long as the plugin has them loaded, which is no
    /// longer than the conversation.
    fn durability(self) -> Option<PermissionLifetime> {
        match self {
            Self::Builtin => None,
            Self::Plugin | Self::Conversation => Some(PermissionLifetime::Conversation),
            Self::Config | Self::Project => Some(PermissionLifetime::Project),
            Self::Global => Some(PermissionLifetime::Global),
        }
    }
}

/// How durable a lifetime is, so coverage can be compared against a grant.
fn lifetime_rank(lifetime: &PermissionLifetime) -> u8 {
    match lifetime {
        PermissionLifetime::Once => 0,
        PermissionLifetime::Conversation => 1,
        PermissionLifetime::Project => 2,
        PermissionLifetime::Global => 3,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPresentation {
    pub action: String,
    pub risk: PermissionRisk,
    pub risk_summary: String,
    pub resources: Vec<PermissionResourcePresentation>,
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

    fn of(effect: &StructuredPermissionEffect) -> Self {
        match effect {
            StructuredPermissionEffect::Allow => Self::Allow,
            StructuredPermissionEffect::Ask => Self::Ask,
            StructuredPermissionEffect::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SelectedInputError {
    #[error("JSON pointer must be empty or begin with '/': {0:?}")]
    InvalidPointer(String),
    #[error("JSON pointer contains an invalid '~' escape: {0:?}")]
    InvalidEscape(String),
    #[error("JSON pointer uses a non-canonical array index: {0:?}")]
    InvalidArrayIndex(String),
    #[error("JSON pointer does not exist in the input: {0:?}")]
    MissingPointer(String),
    #[error("JSON pointer is selected more than once: {0:?}")]
    DuplicatePointer(String),
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

    pub fn option_rule(
        &self,
        option_id: &str,
        lifetime: PermissionLifetime,
    ) -> Option<StructuredPermissionRule> {
        let option = self.options.iter().find(|option| option.id == option_id)?;
        if !option.allowed_lifetimes.contains(&lifetime) {
            return None;
        }
        let mut rule = option.rule.clone();
        rule.lifetime = lifetime;
        Some(rule)
    }

    /// The one rule a per-row answer stands for, or `None` when no row asked to
    /// be remembered.
    ///
    /// Composing is sound because a rule's resource constraints are a set: each
    /// granted row contributes the constraint it chose, and the rule covers a
    /// resource when any constraint does. Rows left ungranted contribute
    /// nothing, which is what makes them this call only — the call itself
    /// proceeds on the answer, not on the rule.
    ///
    /// A written pattern is authority the request never offered, so it is
    /// admitted only against the command on its own row and only for a lifetime
    /// that row's offered rung still allows. That second gate is what keeps
    /// plan mode contained without knowing anything about plans.
    pub fn composed_rules(
        &self,
        rows: &[Option<PermissionRowGrant>],
        lifetime: &PermissionLifetime,
    ) -> Result<Vec<StructuredPermissionRule>, ComposedAnswerError> {
        if rows.len() != self.resources.len() {
            return Err(ComposedAnswerError::RowCount {
                named: rows.len(),
                resources: self.resources.len(),
            });
        }
        let subsumed = self.subsumed_rows(rows);
        let mut rules = Vec::new();
        for (index, grant) in rows.iter().enumerate() {
            let Some(grant) = grant else {
                continue;
            };
            // Validated before pruning: a grant the request never offered is an
            // error whether or not it would have been kept.
            let (option, resources) = self.row_reach(index, grant)?;
            if !option.allowed_lifetimes.contains(lifetime) {
                return Err(ComposedAnswerError::LifetimeWithdrawn(option.id.clone()));
            }
            if subsumed[index].is_some() || self.row_adds_nothing(index, grant, lifetime) {
                continue;
            }
            let rule = StructuredPermissionRule {
                subject: self.subject.clone(),
                executor: self.executor.clone(),
                resources,
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: lifetime.clone(),
                effect: StructuredPermissionEffect::Allow,
                family: option.rule.family,
            };
            if !permission_rule_covers_resource(&rule, self, &self.resources[index]) {
                return Err(ComposedAnswerError::Uncovered);
            }
            rules.push(rule);
        }
        Ok(rules)
    }

    /// Whether a row's grant would add nothing to what already covers it.
    ///
    /// Coverage that expires sooner than the lifetime being granted is not
    /// enough: answering for the project on a row only a conversation rule
    /// covers has to file, or the authority disappears with the session. The
    /// builtin allowlist is never enough either, because it is a defeasible
    /// default consulted only where no rule speaks, not a rule.
    ///
    /// Past that a row is redundant only if it reaches no further than the
    /// coverage. Pinning the reviewed command cannot, and neither can naming
    /// the pattern already stored, but widening to any other pattern reaches
    /// commands this prompt is not about.
    fn row_adds_nothing(
        &self,
        index: usize,
        grant: &PermissionRowGrant,
        lifetime: &PermissionLifetime,
    ) -> bool {
        let Some(coverage) = self
            .presentation
            .resources
            .get(index)
            .and_then(|shown| shown.coverage.as_ref())
        else {
            return false;
        };
        if coverage
            .origin
            .durability()
            .is_none_or(|held| lifetime_rank(&held) < lifetime_rank(lifetime))
        {
            return false;
        }
        match grant {
            PermissionRowGrant::Offered(id) => self
                .options
                .iter()
                .find(|option| option.id == *id)
                .and_then(|option| option.group.as_ref())
                .is_some_and(|group| {
                    group.value == EXACT_COMMAND_CHIP || group.value == coverage.authority
                }),
            PermissionRowGrant::Written(pattern) => *pattern == coverage.authority,
        }
    }

    /// The option one row's grant rides on, and the constraints it names.
    ///
    /// Lifetime is deliberately not checked here: how far a grant reaches is a
    /// question about resources, and subsumption has to answer it before any
    /// lifetime has been chosen.
    fn row_reach(
        &self,
        index: usize,
        grant: &PermissionRowGrant,
    ) -> Result<(&PermissionRuleOption, Vec<PermissionResourceConstraint>), ComposedAnswerError>
    {
        let offered = |id: &str| {
            self.options
                .iter()
                .find(|option| {
                    option.id == id
                        && option.rule.effect == StructuredPermissionEffect::Allow
                        && option.group.as_ref().and_then(|group| group.resource) == Some(index)
                })
                .ok_or_else(|| ComposedAnswerError::NotOffered(id.to_owned()))
        };
        let resource = self
            .resources
            .get(index)
            .ok_or(ComposedAnswerError::Uncovered)?;
        match grant {
            PermissionRowGrant::Offered(id) => {
                let option = offered(id)?;
                Ok((option, option.rule.resources.clone()))
            }
            PermissionRowGrant::Written(pattern) => {
                let option = offered(&format!("{COMMAND_EXACT_PREFIX}{index}"))?;
                grade_command_pattern(pattern, &resource.value).map_err(|fault| {
                    ComposedAnswerError::Pattern {
                        command: safe_summary(&resource.value),
                        fault,
                    }
                })?;
                Ok((
                    option,
                    vec![PermissionResourceConstraint {
                        selector: PermissionResourceSelector::CommandPattern {
                            pattern: pattern.clone(),
                        },
                        ..resource_constraint(resource)
                    }],
                ))
            }
        }
    }

    /// One row's grant as a rule of its own, for asking how far it reaches. The
    /// lifetime is the caller's to set; reach does not depend on it.
    fn row_rule(
        &self,
        index: usize,
        grant: &PermissionRowGrant,
    ) -> Option<StructuredPermissionRule> {
        let (option, resources) = self.row_reach(index, grant).ok()?;
        Some(StructuredPermissionRule {
            subject: self.subject.clone(),
            executor: self.executor.clone(),
            resources,
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Once,
            effect: StructuredPermissionEffect::Allow,
            family: option.rule.family,
        })
    }

    /// For each row, the row whose grant already covers it.
    ///
    /// A grant that reaches no further than another row's contributes nothing:
    /// the prompt should not claim it does, and storage should not keep it. Row
    /// `i` is subsumed by `j` when `j` reaches resource `i` and either `i` does
    /// not reach resource `j` — a redundant narrower row, wherever it sits — or
    /// `j` comes first, which is what settles two rows that reach each other so
    /// that exactly one of them survives.
    pub fn subsumed_rows(&self, rows: &[Option<PermissionRowGrant>]) -> Vec<Option<usize>> {
        let reach: Vec<Option<StructuredPermissionRule>> = rows
            .iter()
            .enumerate()
            .map(|(index, grant)| self.row_rule(index, grant.as_ref()?))
            .collect();
        let covers = |row: usize, resource: usize| {
            reach.get(row).and_then(Option::as_ref).is_some_and(|rule| {
                self.resources
                    .get(resource)
                    .is_some_and(|resource| permission_rule_covers_resource(rule, self, resource))
            })
        };
        (0..rows.len())
            .map(|index| {
                reach.get(index)?.as_ref()?;
                (0..rows.len()).find(|&other| {
                    other != index
                        && covers(other, index)
                        && (!covers(index, other) || other < index)
                })
            })
            .collect()
    }
}

pub fn canonical_json(value: &Value) -> String {
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn canonical_json_sha256(value: &Value) -> String {
    hex_encode(&Sha256::digest(canonical_json(value).as_bytes()))
}

pub fn redacted_review_shape(value: &Value) -> Value {
    fn redact(value: &Value, depth: usize) -> Value {
        if depth == REVIEW_MAX_DEPTH {
            return Value::String("<depth-limit>".into());
        }
        match value {
            Value::Null => Value::String("<null>".into()),
            Value::Bool(_) => Value::String("<boolean>".into()),
            Value::Number(_) => Value::String("<number>".into()),
            Value::String(value) => {
                Value::String(format!("<string:{} chars>", value.chars().count()))
            }
            Value::Array(values) => {
                let mut redacted: Vec<_> = values
                    .iter()
                    .take(REVIEW_MAX_ITEMS)
                    .map(|value| redact(value, depth + 1))
                    .collect();
                if values.len() > REVIEW_MAX_ITEMS {
                    redacted.push(Value::String(format!(
                        "<{} items omitted>",
                        values.len() - REVIEW_MAX_ITEMS
                    )));
                }
                Value::Array(redacted)
            }
            Value::Object(values) => {
                let mut redacted = serde_json::Map::new();
                for (index, value) in values.values().take(REVIEW_MAX_ITEMS).enumerate() {
                    redacted.insert(format!("<field:{}>", index + 1), redact(value, depth + 1));
                }
                if values.len() > REVIEW_MAX_ITEMS {
                    redacted.insert(
                        "<omitted>".into(),
                        Value::String(format!(
                            "<{} fields omitted>",
                            values.len() - REVIEW_MAX_ITEMS
                        )),
                    );
                }
                Value::Object(redacted)
            }
        }
    }

    redact(value, 0)
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("strings always serialize"));
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("object keys always serialize"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

pub fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

pub fn json_pointer<S: AsRef<str>>(segments: &[S]) -> String {
    let mut pointer = String::new();
    for segment in segments {
        pointer.push('/');
        pointer.push_str(&escape_json_pointer_segment(segment.as_ref()));
    }
    pointer
}

pub fn selected_input_pointer<'a>(
    input: &'a Value,
    pointer: &str,
) -> Result<&'a Value, SelectedInputError> {
    let segments = decode_json_pointer(pointer)?;
    let mut selected = input;
    for segment in segments {
        selected = match selected {
            Value::Object(object) => object
                .get(&segment)
                .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?,
            Value::Array(array) => {
                if segment == "-"
                    || (segment.len() > 1 && segment.starts_with('0'))
                    || !segment.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(SelectedInputError::InvalidArrayIndex(pointer.to_owned()));
                }
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| SelectedInputError::InvalidArrayIndex(pointer.to_owned()))?;
                array
                    .get(index)
                    .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?
            }
            _ => return Err(SelectedInputError::MissingPointer(pointer.to_owned())),
        };
    }
    Ok(selected)
}

pub fn selected_input<S: AsRef<str>>(
    input: &Value,
    pointers: &[S],
) -> Result<Vec<SelectedPermissionArgument>, SelectedInputError> {
    let mut seen = HashSet::with_capacity(pointers.len());
    let mut selected = Vec::with_capacity(pointers.len());
    for pointer in pointers {
        let pointer = pointer.as_ref();
        if !seen.insert(pointer) {
            return Err(SelectedInputError::DuplicatePointer(pointer.to_owned()));
        }
        let value = selected_input_pointer(input, pointer)?.clone();
        selected.push(SelectedPermissionArgument {
            pointer: pointer.to_owned(),
            digest: canonical_json_sha256(&value),
            value,
        });
    }
    Ok(selected)
}

pub fn selected_input_digest<S: AsRef<str>>(
    input: &Value,
    pointers: &[S],
) -> Result<String, SelectedInputError> {
    let mut seen = HashSet::with_capacity(pointers.len());
    let mut projection = Vec::with_capacity(pointers.len());
    for pointer in pointers {
        let pointer = pointer.as_ref();
        if pointer.is_empty() || !seen.insert(pointer) {
            return Err(if pointer.is_empty() {
                SelectedInputError::InvalidPointer(pointer.to_owned())
            } else {
                SelectedInputError::DuplicatePointer(pointer.to_owned())
            });
        }
        decode_json_pointer(pointer)?;
        let selected = selected_input_pointer(input, pointer);
        projection.push(serde_json::json!({
            "pointer": pointer,
            "present": selected.is_ok(),
            "value": selected.ok(),
        }));
    }
    Ok(canonical_json_sha256(&Value::Array(projection)))
}

fn decode_json_pointer(pointer: &str) -> Result<Vec<String>, SelectedInputError> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(pointer) = pointer.strip_prefix('/') else {
        return Err(SelectedInputError::InvalidPointer(pointer.to_owned()));
    };
    pointer
        .split('/')
        .map(|segment| {
            let mut decoded = String::with_capacity(segment.len());
            let mut chars = segment.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => return Err(SelectedInputError::InvalidEscape(pointer.to_owned())),
                }
            }
            Ok(decoded)
        })
        .collect()
}

pub fn argument_constraint_matches(
    constraint: &PermissionArgumentConstraint,
    input: &Value,
) -> bool {
    match constraint {
        PermissionArgumentConstraint::Exact { digest } => canonical_json_sha256(input) == *digest,
        PermissionArgumentConstraint::Selected { arguments } => {
            let mut seen = HashSet::with_capacity(arguments.len());
            arguments.iter().all(|argument| {
                seen.insert(argument.pointer.as_str())
                    && canonical_json_sha256(&argument.value) == argument.digest
                    && selected_input_pointer(input, &argument.pointer).is_ok_and(|selected| {
                        canonical_json_sha256(selected) == argument.digest
                            && canonical_json(selected) == canonical_json(&argument.value)
                    })
            })
        }
        PermissionArgumentConstraint::SelectedDigest { pointers, digest } => {
            selected_input_digest(input, pointers).is_ok_and(|actual| actual == *digest)
        }
        PermissionArgumentConstraint::Unconstrained => true,
    }
}

pub fn resource_constraint_matches(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    constraint_covers_resource(constraint, resource, None, RuleIntent::Grant)
}

/// What a rule does with a resource it names. Protection raises the bar for
/// granting only: a refusal that had to clear the same bar would fail open on
/// exactly the resources protection exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleIntent {
    Grant,
    Restrain,
}

impl RuleIntent {
    fn of(effect: &StructuredPermissionEffect) -> Self {
        match effect {
            StructuredPermissionEffect::Allow => Self::Grant,
            StructuredPermissionEffect::Ask | StructuredPermissionEffect::Deny => Self::Restrain,
        }
    }
}

/// Whether the constraint and the resource name the same operation. Without a
/// family this is exact equality on kind and access; a family instead accepts
/// any member pair, which is what lets one subtree grant serve read, list, and
/// search without ever reaching a write.
fn operation_matches(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
    family: Option<PermissionCapabilityFamily>,
) -> bool {
    match family {
        Some(PermissionCapabilityFamily::FilesystemRead) => {
            is_filesystem_read_kind(&constraint.kind)
                && is_filesystem_read_kind(&resource.kind)
                && is_filesystem_read_access(constraint.access.as_ref())
                && is_filesystem_read_access(resource.access.as_ref())
        }
        // Widens which subject a rule reaches, never which operation, so the
        // constraint still has to name the operation exactly.
        Some(PermissionCapabilityFamily::McpServer) | None => {
            constraint.kind == resource.kind
                && !constraint
                    .access
                    .as_ref()
                    .is_some_and(|access| resource.access.as_ref() != Some(access))
        }
    }
}

fn is_filesystem_read_kind(kind: &PermissionResourceKind) -> bool {
    matches!(
        kind,
        PermissionResourceKind::File | PermissionResourceKind::Directory
    )
}

/// An absent access means "any access" on a constraint, which would reach
/// `Write`, so only an explicit read-shaped access joins the family.
fn is_filesystem_read_access(access: Option<&PermissionResourceAccess>) -> bool {
    matches!(
        access,
        Some(PermissionResourceAccess::Read) | Some(PermissionResourceAccess::Search)
    )
}

fn constraint_covers_resource(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
    family: Option<PermissionCapabilityFamily>,
    intent: RuleIntent,
) -> bool {
    if !operation_matches(constraint, resource, family) {
        return false;
    }
    if constraint
        .protected
        .is_some_and(|protected| resource.protected != protected)
    {
        return false;
    }
    if resource.protected
        && intent == RuleIntent::Grant
        && !protected_coverage_allowed(constraint, resource)
    {
        return false;
    }
    if !selector_matches(&constraint.selector, &resource.value, &resource.kind) {
        return false;
    }
    constraint.attributes.iter().all(|(name, selector)| {
        resource
            .attributes
            .get(name)
            .is_some_and(|value| selector_matches(selector, value, &attribute_kind(name)))
    })
}

/// Reports whether a constraint is specific enough to cover a protected resource.
///
/// Protected resources normally demand an exact or digest selector with every
/// attribute pinned the same way. Protected commands additionally accept the
/// blanket `Any` selector, which reaches a saved rule only through a typed
/// broad shell confirmation, so redirects and heredocs stop prompting once the
/// user grants arbitrary command execution. Command patterns stay excluded
/// because the reviewed text of a protected command describes more than the
/// pattern does.
fn protected_coverage_allowed(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    if resource.kind == PermissionResourceKind::Command
        && matches!(constraint.selector, PermissionResourceSelector::Any)
    {
        return true;
    }
    constraint.protected == Some(true)
        && matches!(
            constraint.selector,
            PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. }
        )
        && constraint.attributes.len() == resource.attributes.len()
        && constraint.attributes.values().all(|selector| {
            matches!(
                selector,
                PermissionResourceSelector::Exact { .. }
                    | PermissionResourceSelector::Digest { .. }
            )
        })
}

/// How narrowly a selector names a resource. A rule that names one resource
/// outranks one that names a region, which outranks one that names everything,
/// so a specific grant is not swallowed by a broad ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SelectorWidth {
    Blanket,
    /// How much literal text the selector pins, as a token count and their total
    /// byte length, so `git status *` outranks `git *`. A subtree pins a region
    /// by structure rather than by text and so pins none.
    Region(usize, usize),
    Exact,
}

fn selector_width(selector: &PermissionResourceSelector) -> SelectorWidth {
    match selector {
        PermissionResourceSelector::Any => SelectorWidth::Blanket,
        // The grammar knows to leave the trailing wildcard out of the count,
        // which a plain tokenization would include.
        PermissionResourceSelector::CommandPattern { pattern } => {
            match super::command_pattern::specificity(pattern) {
                Some((tokens, bytes)) => pinned_text_width(tokens, bytes),
                None => SelectorWidth::Blanket,
            }
        }
        PermissionResourceSelector::Prefix { value } => pinned_text_width(
            value.split_whitespace().count(),
            value.split_whitespace().map(str::len).sum(),
        ),
        PermissionResourceSelector::Subtree { .. }
        | PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::UrlSubtreeDigest { .. }
        | PermissionResourceSelector::UrlOriginDigest { .. } => SelectorWidth::Region(0, 0),
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            SelectorWidth::Exact
        }
    }
}

/// A selector pinning no text reaches everything its kind has, so it ranks with
/// the blanket selector rather than above it. Prefixes and command patterns are
/// measured the same way, because a configured scope becomes one or the other
/// purely by its spelling and the two must rank against each other honestly.
fn pinned_text_width(tokens: usize, bytes: usize) -> SelectorWidth {
    if tokens == 0 {
        SelectorWidth::Blanket
    } else {
        SelectorWidth::Region(tokens, bytes)
    }
}

/// Where a rule stands relative to the others once it has matched. Ordered
/// lexicographically: a denial is absolute and outranks any width, then the rule
/// that names the least wins, and an ask breaks a tie against an allow.
type RuleStanding = (bool, SelectorWidth, StructuredPermissionDecision);

/// Reports how strongly a rule speaks to a resource, or `None` when it does not
/// speak to it at all. Keeping the traversal separate from the effect is what
/// lets one definition of matching answer for allow, deny, and ask alike.
///
/// A rule that names no resource is unrestricted, which is how the picker
/// presents it and how deny already reads it, so it speaks to every resource at
/// the widest rank.
fn rule_standing(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> Option<RuleStanding> {
    rule_reach(rule, request, resource).map(|(standing, _)| standing)
}

/// `rule_standing` with the constraint that carried the standing, so a caller
/// that has to name the authority names the one that actually decided. A rule
/// naming no resource is unrestricted and so carries no constraint.
fn rule_reach<'a>(
    rule: &'a StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> Option<(RuleStanding, Option<&'a PermissionResourceConstraint>)> {
    if !rule_context_matches(rule, request) {
        return None;
    }
    let decision = StructuredPermissionDecision::of(&rule.effect);
    let standing = |width| {
        (
            decision == StructuredPermissionDecision::Deny,
            width,
            decision,
        )
    };
    if rule.resources.is_empty() {
        return Some((standing(SelectorWidth::Blanket), None));
    }
    rule.resources
        .iter()
        .filter(|constraint| {
            constraint_covers_resource(
                constraint,
                resource,
                rule.family,
                RuleIntent::of(&rule.effect),
            )
        })
        .max_by_key(|constraint| selector_width(&constraint.selector))
        .map(|constraint| {
            (
                standing(selector_width(&constraint.selector)),
                Some(constraint),
            )
        })
}

/// What the rule set says about one resource, and the allow that covers it.
///
/// Ranking is what keeps a config that asks about `git *` from swallowing its
/// own `git status` allow. Ties are broken entirely by the standing, so the
/// answer does not depend on rule order. The credited allow is ranked the same
/// way, so the authority a prompt names is the one that would decide.
pub fn permission_rules_resource_standing(
    rules: &[PolicyRule],
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> ResourceStanding {
    let decision = resource_decision(rules.iter().map(|policy| &policy.rule), request, resource);
    // `min_by_key` over the reversed width keeps the first rule of equal reach,
    // so a grant the user made outranks configured policy that says the same.
    let coverage = rules
        .iter()
        .filter(|policy| policy.rule.effect == StructuredPermissionEffect::Allow)
        .filter_map(|policy| {
            rule_reach(&policy.rule, request, resource)
                .map(|((_, width, _), constraint)| (policy, width, constraint))
        })
        .min_by_key(|(_, width, _)| Reverse(*width))
        .map(|(policy, _, constraint)| ResourceCoverage {
            origin: policy.origin,
            authority: constraint.map_or_else(
                || blanket_authority(&resource.kind),
                |constraint| selector_authority(&constraint.selector, &resource.kind),
            ),
        });
    ResourceStanding { decision, coverage }
}

fn resource_decision<'a>(
    rules: impl IntoIterator<Item = &'a StructuredPermissionRule>,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> StructuredPermissionDecision {
    rules
        .into_iter()
        .filter_map(|rule| rule_standing(rule, request, resource))
        .max()
        .map_or(StructuredPermissionDecision::NoMatch, |(_, _, decision)| {
            decision
        })
}

/// How a covering constraint names a resource, for a prompt that has to say why
/// the resource is already allowed.
fn selector_authority(
    selector: &PermissionResourceSelector,
    kind: &PermissionResourceKind,
) -> String {
    match selector {
        PermissionResourceSelector::CommandPattern { pattern } => safe_summary(pattern),
        PermissionResourceSelector::Prefix { value } => safe_summary(value),
        PermissionResourceSelector::Subtree { root } => safe_summary(root),
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            format!("this {}", kind_noun(kind))
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::UrlSubtreeDigest { .. }
        | PermissionResourceSelector::UrlOriginDigest { .. } => {
            format!("this {} tree", kind_noun(kind))
        }
        PermissionResourceSelector::Any => blanket_authority(kind),
    }
}

fn blanket_authority(kind: &PermissionResourceKind) -> String {
    format!("any {}", kind_noun(kind))
}

fn kind_noun(kind: &PermissionResourceKind) -> String {
    match kind {
        PermissionResourceKind::Custom { name } => safe_summary(name),
        other => format!("{other:?}").to_lowercase(),
    }
}

pub fn permission_rule_covers_request(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    rule.effect == StructuredPermissionEffect::Allow
        && rule_context_matches(rule, request)
        && request.resources.iter().all(|resource| {
            rule.resources.iter().any(|constraint| {
                constraint_covers_resource(
                    constraint,
                    resource,
                    rule.family,
                    RuleIntent::of(&rule.effect),
                )
            })
        })
}

pub fn permission_rule_covers_resource(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> bool {
    rule.effect == StructuredPermissionEffect::Allow
        && rule_standing(rule, request, resource).is_some()
}

pub fn permission_rules_cover_request(
    rules: &[StructuredPermissionRule],
    request: &PermissionRequest,
) -> bool {
    rules.iter().any(|rule| {
        rule.effect == StructuredPermissionEffect::Allow && rule_context_matches(rule, request)
    }) && request.resources.iter().all(|resource| {
        rules
            .iter()
            .any(|rule| permission_rule_covers_resource(rule, request, resource))
    })
}

pub fn permission_rule_intersects_request(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    rule.effect == StructuredPermissionEffect::Deny
        && rule_context_matches(rule, request)
        && (rule.resources.is_empty()
            || request.resources.iter().any(|resource| {
                rule.resources.iter().any(|constraint| {
                    constraint_covers_resource(
                        constraint,
                        resource,
                        rule.family,
                        RuleIntent::of(&rule.effect),
                    )
                })
            }))
}

/// What the rule set says about a whole request.
///
/// Deny and ask propagate from any one resource, because narrowing anywhere
/// narrows the call. Allow does not: it needs every resource covered, since a
/// call is only authorized when nothing it touches is left unspoken for.
pub fn evaluate_structured_permission_rules(
    rules: &[StructuredPermissionRule],
    request: &PermissionRequest,
) -> StructuredPermissionDecision {
    let narrowed = request
        .resources
        .iter()
        .map(|resource| resource_decision(rules, request, resource))
        .fold(
            StructuredPermissionDecision::NoMatch,
            |decision, resource| decision.merge(resource),
        );
    match narrowed {
        StructuredPermissionDecision::Deny | StructuredPermissionDecision::Ask => narrowed,
        StructuredPermissionDecision::Allow | StructuredPermissionDecision::NoMatch => {
            if permission_rules_cover_request(rules, request) {
                StructuredPermissionDecision::Allow
            } else {
                StructuredPermissionDecision::NoMatch
            }
        }
    }
}

fn rule_context_matches(rule: &StructuredPermissionRule, request: &PermissionRequest) -> bool {
    subject_matches(rule, request)
        && rule.executor == request.executor
        && argument_constraint_matches(&rule.arguments, &request.input)
}

/// Authority is keyed to the subject, which for a first-party tool is a single
/// contract. A family rule is instead keyed to the trust domain, so the contract
/// that happened to ask first stops being part of the key. Both sides must be
/// members, which pins the owner too, since membership names one owner.
fn subject_matches(rule: &StructuredPermissionRule, request: &PermissionRequest) -> bool {
    if rule.subject == request.subject {
        return true;
    }
    match rule.family {
        Some(PermissionCapabilityFamily::FilesystemRead) => {
            is_filesystem_read_subject(&rule.subject)
                && is_filesystem_read_subject(&request.subject)
        }
        Some(PermissionCapabilityFamily::McpServer) => mcp_server(&rule.subject)
            .zip(mcp_server(&request.subject))
            .is_some_and(|(rule, request)| rule == request),
        None => false,
    }
}

/// A server-wide rule is keyed to the server, so the tool that happened to ask
/// first stops being part of the key. An empty name is never a key: it is what
/// a record written before servers were recorded deserializes to, and matching
/// on it would let one such rule reach every server.
fn mcp_server(subject: &PermissionSubject) -> Option<&str> {
    match subject {
        PermissionSubject::Mcp { server, .. } if !server.is_empty() => Some(server),
        PermissionSubject::Mcp { .. }
        | PermissionSubject::Native { .. }
        | PermissionSubject::Lua { .. }
        | PermissionSubject::UnknownLegacy { .. } => None,
    }
}

fn is_filesystem_read_subject(subject: &PermissionSubject) -> bool {
    match subject {
        PermissionSubject::Native { owner, contract } => {
            owner == WORKCELL_OWNER && FILESYSTEM_READ_CONTRACTS.contains(&contract.as_str())
        }
        PermissionSubject::Lua { .. }
        | PermissionSubject::Mcp { .. }
        | PermissionSubject::UnknownLegacy { .. } => false,
    }
}

fn selector_matches(
    selector: &PermissionResourceSelector,
    value: &str,
    kind: &PermissionResourceKind,
) -> bool {
    match selector {
        PermissionResourceSelector::Any => true,
        PermissionResourceSelector::Digest { digest } => {
            resource_value_digest(value, kind).is_some_and(|actual| actual == *digest)
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { digest } => {
            matches!(
                kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            ) && filesystem_ancestor_digests(value).is_some_and(|digests| digests.contains(digest))
        }
        PermissionResourceSelector::UrlSubtreeDigest { digest } => {
            matches!(kind, PermissionResourceKind::Url)
                && url_subtree_digests(value).is_some_and(|digests| digests.contains(digest))
        }
        PermissionResourceSelector::UrlOriginDigest { digest } => {
            matches!(kind, PermissionResourceKind::Url)
                && url_origin_digest(value).is_some_and(|actual| actual == *digest)
        }
        PermissionResourceSelector::CommandPattern { pattern } => {
            matches!(kind, PermissionResourceKind::Command)
                && super::command_pattern::matches(pattern, value)
        }
        PermissionResourceSelector::Exact { value: expected } => match kind {
            PermissionResourceKind::File | PermissionResourceKind::Directory => {
                normalized_filesystem_path(expected)
                    .zip(normalized_filesystem_path(value))
                    .is_some_and(|(expected, actual)| expected == actual)
            }
            PermissionResourceKind::Url => strict_http_url(expected)
                .zip(strict_http_url(value))
                .is_some_and(|(expected, actual)| expected.key == actual.key),
            _ => expected == value,
        },
        PermissionResourceSelector::Subtree { root } => match kind {
            PermissionResourceKind::File | PermissionResourceKind::Directory => {
                normalized_filesystem_path(root)
                    .zip(normalized_filesystem_path(value))
                    .is_some_and(|(root, value)| value == root || value.starts_with(root))
            }
            PermissionResourceKind::Url => http_url_is_subtree(root, value),
            _ => false,
        },
        // Raw text matched raw, on every kind, exactly as a configured scope
        // ending in a bare `*` always was. Normalizing either side would change
        // which existing configs match, and a deny is among them.
        PermissionResourceSelector::Prefix { value: prefix } => value.starts_with(prefix),
    }
}

fn resource_value_digest(value: &str, kind: &PermissionResourceKind) -> Option<String> {
    let canonical = match kind {
        PermissionResourceKind::File | PermissionResourceKind::Directory => {
            normalized_filesystem_path(value)?
                .to_string_lossy()
                .into_owned()
        }
        PermissionResourceKind::Url => strict_http_url(value)?.key,
        _ => value.to_owned(),
    };
    Some(canonical_json_sha256(&Value::String(canonical)))
}

fn scoped_digest(domain: &str, value: &str) -> String {
    canonical_json_sha256(&serde_json::json!([domain, value]))
}

fn filesystem_subtree_digest(value: &str) -> Option<String> {
    let value = normalized_filesystem_path(value)?;
    Some(scoped_digest(
        "filesystem_subtree",
        &value.to_string_lossy(),
    ))
}

fn filesystem_ancestor_digests(value: &str) -> Option<HashSet<String>> {
    let value = normalized_filesystem_path(value)?;
    Some(
        value
            .ancestors()
            .map(|ancestor| scoped_digest("filesystem_subtree", &ancestor.to_string_lossy()))
            .collect(),
    )
}

fn url_subtree_digest(root: &str) -> String {
    scoped_digest("url_subtree", root)
}

fn url_subtree_digests(value: &str) -> Option<HashSet<String>> {
    Some(
        url_subtree_roots(&strict_http_url(value)?)?
            .iter()
            .map(|root| url_subtree_digest(root))
            .collect(),
    )
}

fn url_origin_digest(value: &str) -> Option<String> {
    let strict = strict_http_url(value)?;
    Some(scoped_digest(
        "url_origin",
        &strict.url.origin().ascii_serialization(),
    ))
}

/// Every prefix of a URL's path as a subtree root, deepest first and ending at
/// the origin's own root. One construction, so a rule minted for a root and the
/// match that has to accept it can never disagree on how the root is spelled.
fn url_subtree_roots(strict: &StrictHttpUrl) -> Option<Vec<String>> {
    let segments: Vec<_> = strict
        .url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut base = strict.url.clone();
    base.set_query(None);
    base.set_fragment(None);
    Some(
        (0..=segments.len())
            .rev()
            .map(|depth| {
                let mut root = base.clone();
                root.set_path(&format!("/{}", segments[..depth].join("/")));
                normalize_percent_hex(root.as_str())
            })
            .collect(),
    )
}

fn normalized_filesystem_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let path = Path::new(path);
    let absolute = std::path::absolute(path).ok()?;
    Some(
        caudra_storage::paths::incremental_canonicalize(&absolute)
            .unwrap_or_else(|| caudra_storage::paths::normalize_path(&absolute)),
    )
}

struct StrictHttpUrl {
    url: Url,
    key: String,
}

fn strict_http_url(value: &str) -> Option<StrictHttpUrl> {
    if value.chars().any(char::is_control) || value.contains('\\') {
        return None;
    }
    let authority_and_path = value.split_once("://")?.1;
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    validate_url_percent_encoding(value)?;
    let mut url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    url.set_fragment(None);
    let key = normalize_percent_hex(url.as_str());
    Some(StrictHttpUrl { url, key })
}

fn validate_url_percent_encoding(value: &str) -> Option<()> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1)?;
        let low = *bytes.get(index + 2)?;
        let decoded = (hex_value(high)? << 4) | hex_value(low)?;
        if decoded == b'/'
            || decoded == b'\\'
            || decoded == b'.'
            || decoded == b'%'
            || decoded <= 0x1f
            || decoded == 0x7f
            || decoded.is_ascii_alphanumeric()
            || matches!(decoded, b'-' | b'_' | b'~')
        {
            return None;
        }
        index += 3;
    }
    Some(())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn normalize_percent_hex(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            output.push('%');
            output.push((bytes[index + 1] as char).to_ascii_uppercase());
            output.push((bytes[index + 2] as char).to_ascii_uppercase());
            index += 3;
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

fn http_url_is_subtree(root: &str, value: &str) -> bool {
    let Some(root) = strict_http_url(root) else {
        return false;
    };
    let Some(value) = strict_http_url(value) else {
        return false;
    };
    if root.url.query().is_some()
        || root.url.scheme() != value.url.scheme()
        || root.url.host_str() != value.url.host_str()
        || root.url.port_or_known_default() != value.url.port_or_known_default()
    {
        return false;
    }
    let root_path = normalize_percent_hex(root.url.path());
    let value_path = normalize_percent_hex(value.url.path());
    if root_path == "/" {
        return true;
    }
    let root_path = root_path.trim_end_matches('/');
    value_path == root_path
        || value_path
            .strip_prefix(root_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn subject_and_executor(tool: &ToolKey) -> (PermissionSubject, PermissionExecutorKind) {
    match tool {
        ToolKey::Native(name) => (
            PermissionSubject::Native {
                owner: NATIVE_OWNER.into(),
                contract: name.to_string(),
            },
            PermissionExecutorKind::Native,
        ),
        ToolKey::McpTool { server, tool } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: tool.to_string(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::McpServer { server } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: "*".into(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::Wildcard => (
            PermissionSubject::UnknownLegacy {
                identity: tool.to_string(),
            },
            PermissionExecutorKind::UnknownLegacy,
        ),
    }
}

fn risk_for(tool: &ToolKey, force_prompt: bool) -> PermissionRisk {
    if force_prompt {
        return PermissionRisk::Critical;
    }
    match tool {
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => PermissionRisk::High,
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "bash" => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "webfetch" => PermissionRisk::Medium,
        ToolKey::Native(name) if name.as_ref() == "websearch" => PermissionRisk::Low,
        ToolKey::Native(name)
            if FILE_READ_TOOLS.contains(&name.as_ref())
                || DIRECTORY_READ_TOOLS.contains(&name.as_ref())
                || FILE_SEARCH_TOOLS.contains(&name.as_ref()) =>
        {
            PermissionRisk::Low
        }
        ToolKey::Native(_) | ToolKey::Wildcard => PermissionRisk::Unknown,
    }
}

fn legacy_authority_profile(tool: &ToolKey) -> PermissionAuthorityProfile {
    match tool {
        ToolKey::Native(name) if name.as_ref() == "webfetch" => PermissionAuthorityProfile::Url,
        ToolKey::Native(name) if name.as_ref() == "websearch" => PermissionAuthorityProfile::Query,
        ToolKey::Native(name) if name.as_ref() == "bash" => PermissionAuthorityProfile::Shell,
        ToolKey::Native(name) if FILE_SEARCH_TOOLS.contains(&name.as_ref()) => {
            PermissionAuthorityProfile::Filesystem {
                input_pointers: vec!["/pattern".into()],
            }
        }
        ToolKey::Native(name)
            if FILE_WRITE_TOOLS.contains(&name.as_ref())
                || FILE_READ_TOOLS.contains(&name.as_ref())
                || DIRECTORY_READ_TOOLS.contains(&name.as_ref()) =>
        {
            PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            }
        }
        _ => PermissionAuthorityProfile::ExactOnly,
    }
}

fn resources_for(
    tool: &ToolKey,
    scopes: &[String],
    input: &Value,
    cwd: &Path,
    force_prompt: bool,
) -> Vec<PermissionResource> {
    match tool {
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                filesystem_resource(
                    PermissionResourceKind::File,
                    scope,
                    PermissionResourceAccess::Write,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "bash" => {
            let default_workdir = input
                .get("workdir")
                .and_then(Value::as_str)
                .and_then(normalized_filesystem_path)
                .unwrap_or_else(|| cwd.to_path_buf())
                .to_string_lossy()
                .into_owned();
            scopes
                .iter()
                .map(|scope| {
                    let (command, workdir) = super::bash_scope_parts(scope)
                        .map(|(command, workdir)| (command.to_owned(), workdir.to_owned()))
                        .unwrap_or_else(|| (scope.clone(), default_workdir.clone()));
                    PermissionResource {
                        kind: PermissionResourceKind::Command,
                        value: command,
                        access: Some(PermissionResourceAccess::Execute),
                        protected: force_prompt,
                        requires_prompt: force_prompt,
                        attributes: BTreeMap::from([("workdir".into(), workdir)]),
                    }
                })
                .collect()
        }
        ToolKey::Native(name) if name.as_ref() == "webfetch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Url,
                value: strict_http_url(scope)
                    .map(|strict| strict.key)
                    .unwrap_or_else(|| scope.clone()),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "websearch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Query,
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if FILE_READ_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                let directory = name.as_ref() == "file_index" && Path::new(scope).is_dir();
                filesystem_resource(
                    if directory {
                        PermissionResourceKind::Directory
                    } else {
                        PermissionResourceKind::File
                    },
                    scope,
                    PermissionResourceAccess::Read,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if DIRECTORY_READ_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                filesystem_resource(
                    PermissionResourceKind::Directory,
                    scope,
                    PermissionResourceAccess::Read,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if FILE_SEARCH_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                let root = scope.strip_suffix("/**").unwrap_or(scope);
                filesystem_resource(
                    PermissionResourceKind::Directory,
                    root,
                    PermissionResourceAccess::Search,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "memory" => {
            let write = matches!(
                input.get("command").and_then(Value::as_str),
                Some("write" | "delete")
            );
            scopes
                .iter()
                .map(|scope| {
                    let subtree = scope.strip_suffix("/**");
                    PermissionResource {
                        kind: if subtree.is_some() {
                            PermissionResourceKind::Directory
                        } else {
                            PermissionResourceKind::File
                        },
                        value: subtree.unwrap_or(scope).to_owned(),
                        access: Some(if write {
                            PermissionResourceAccess::Write
                        } else {
                            PermissionResourceAccess::Read
                        }),
                        protected: false,
                        requires_prompt: false,
                        attributes: BTreeMap::new(),
                    }
                })
                .collect()
        }
        ToolKey::Native(name) if name.as_ref() == "skill" => scopes
            .iter()
            .map(|scope| {
                let subtree = scope.strip_suffix("/**");
                PermissionResource {
                    kind: if subtree.is_some() {
                        PermissionResourceKind::Directory
                    } else {
                        PermissionResourceKind::File
                    },
                    value: subtree.unwrap_or(scope).to_owned(),
                    access: Some(PermissionResourceAccess::Read),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }
            })
            .collect(),
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => vec![PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: "mcp_tool".into(),
            },
            value: tool.to_string(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        _ => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: tool.to_string(),
                },
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Execute),
                protected: force_prompt,
                requires_prompt: force_prompt,
                attributes: BTreeMap::new(),
            })
            .collect(),
    }
}

fn filesystem_resource(
    kind: PermissionResourceKind,
    value: &str,
    access: PermissionResourceAccess,
    cwd: &Path,
) -> PermissionResource {
    let (protected, requires_prompt) = filesystem_resource_flags(value, &access, cwd);
    PermissionResource {
        kind,
        value: value.to_owned(),
        access: Some(access),
        protected,
        requires_prompt,
        attributes: BTreeMap::new(),
    }
}

pub fn filesystem_permission_resource(
    kind: PermissionResourceKind,
    path: &Path,
    access: PermissionResourceAccess,
    cwd: &Path,
) -> PermissionResource {
    filesystem_resource(kind, &path.to_string_lossy(), access, cwd)
}

fn filesystem_resource_flags(
    value: &str,
    access: &PermissionResourceAccess,
    cwd: &Path,
) -> (bool, bool) {
    let Some(value) = normalized_filesystem_path(value) else {
        return (true, true);
    };
    let project = normalized_filesystem_path(&cwd.to_string_lossy());
    if matches!(
        access,
        PermissionResourceAccess::Read | PermissionResourceAccess::Search
    ) && project
        .as_deref()
        .is_some_and(|project| is_inert_git_metadata(&value, project))
    {
        return (false, false);
    }
    let outside_project = project
        .as_deref()
        .is_none_or(|project| value != project && !value.starts_with(project));
    let protected = value.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        matches!(component.as_ref(), GIT_METADATA_DIR | ".ssh" | ".aws")
            || component == ".env"
            || component.starts_with(".env.")
    });
    (protected, protected || outside_project)
}

/// Reports whether a path is the project's own inert git bookkeeping.
///
/// Reading these reveals no secret and changes no state, so they are exempt
/// from the `.git` guard that otherwise forces a prompt on every path holding a
/// credential-bearing component. `config` and `hooks` are deliberately absent:
/// remote URLs in `config` embed tokens, and hooks are executable. Anything
/// unrecognized stays protected, so a new git file is guarded until reviewed.
fn is_inert_git_metadata(path: &Path, project: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project) else {
        return false;
    };
    let mut components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned());
    components.next().as_deref() == Some(GIT_METADATA_DIR)
        && components
            .next()
            .is_some_and(|entry| INERT_GIT_METADATA.contains(&entry.as_str()))
}

/// The digest a selector pins a value with, falling back to hashing the value
/// as text when the kind has no canonical form of its own.
fn pinned_digest(value: &str, kind: &PermissionResourceKind) -> String {
    resource_value_digest(value, kind)
        .unwrap_or_else(|| canonical_json_sha256(&Value::String(value.to_owned())))
}

/// The kind an attribute's value is matched as. One definition, so building a
/// constraint and testing one agree on what `workdir` means.
fn attribute_kind(name: &str) -> PermissionResourceKind {
    if name == WORKDIR_ATTRIBUTE {
        PermissionResourceKind::Directory
    } else if name == NORMALIZED_COMMAND_ATTRIBUTE {
        PermissionResourceKind::Command
    } else {
        PermissionResourceKind::Custom {
            name: name.to_owned(),
        }
    }
}

/// The constraint that pins one resource to itself, attributes included.
///
/// Widening a single resource means taking this and replacing its selector, so
/// option generation and answer validation both start here.
fn resource_constraint(resource: &PermissionResource) -> PermissionResourceConstraint {
    PermissionResourceConstraint {
        kind: resource.kind.clone(),
        selector: PermissionResourceSelector::Digest {
            digest: pinned_digest(&resource.value, &resource.kind),
        },
        access: resource.access.clone(),
        protected: Some(resource.protected),
        attributes: resource
            .attributes
            .iter()
            .filter(|(name, _)| name.as_str() != NORMALIZED_COMMAND_ATTRIBUTE)
            .map(|(name, value)| {
                (
                    name.clone(),
                    PermissionResourceSelector::Digest {
                        digest: pinned_digest(value, &attribute_kind(name)),
                    },
                )
            })
            .collect(),
    }
}

fn exact_resource_constraints(
    resources: &[PermissionResource],
) -> Vec<PermissionResourceConstraint> {
    resources.iter().map(resource_constraint).collect()
}

#[allow(clippy::too_many_arguments)]
fn rule_options(
    tool: &ToolKey,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    resources: &[PermissionResource],
    input: &Value,
    input_digest: &str,
    cwd: &Path,
    authority: &PermissionAuthorityProfile,
) -> Vec<PermissionRuleOption> {
    let exact_arguments = PermissionArgumentConstraint::Exact {
        digest: input_digest.into(),
    };
    let resource_constraints = exact_resource_constraints(resources);
    let option = |id: &str,
                  label: &str,
                  description: &str,
                  effect: StructuredPermissionEffect,
                  resources: Vec<PermissionResourceConstraint>,
                  arguments: PermissionArgumentConstraint,
                  allowed_lifetimes: Vec<PermissionLifetime>,
                  broad: bool,
                  is_default: bool,
                  confirmation: Option<&str>| PermissionRuleOption {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        rule: StructuredPermissionRule {
            subject: subject.clone(),
            executor: executor.clone(),
            resources,
            arguments,
            lifetime: PermissionLifetime::Once,
            effect,
            family: None,
        },
        allowed_lifetimes,
        broad,
        is_default,
        confirmation: confirmation.map(String::from),
        group: None,
        caution: None,
    };
    let reusable = vec![
        PermissionLifetime::Conversation,
        PermissionLifetime::Project,
        PermissionLifetime::Global,
    ];
    let mut exact_lifetimes = vec![PermissionLifetime::Once];
    exact_lifetimes.extend(reusable.iter().cloned());
    let mut options = vec![
        option(
            "allow_exact",
            "This exact call",
            "Allow only these exact arguments and resources.",
            StructuredPermissionEffect::Allow,
            resource_constraints.clone(),
            exact_arguments.clone(),
            exact_lifetimes,
            false,
            true,
            None,
        ),
        option(
            "deny_exact",
            "Deny this exact call",
            "Deny only these exact arguments and resources.",
            StructuredPermissionEffect::Deny,
            resource_constraints.clone(),
            exact_arguments,
            vec![PermissionLifetime::Project, PermissionLifetime::Global],
            false,
            false,
            None,
        ),
    ];

    if matches!(authority, PermissionAuthorityProfile::Url)
        && let [resource] = resources
        && resource.kind == PermissionResourceKind::Url
        && let Some(strict) = strict_http_url(&resource.value)
    {
        let exact_url = PermissionResourceConstraint {
            kind: PermissionResourceKind::Url,
            selector: PermissionResourceSelector::Digest {
                digest: resource_value_digest(&resource.value, &PermissionResourceKind::Url)
                    .expect("strict URL has a digest"),
            },
            access: resource.access.clone(),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        options.push(option(
            "allow_exact_url",
            "This exact URL",
            "Allow this normalized URL with different fetch format or timeout controls.",
            StructuredPermissionEffect::Allow,
            vec![exact_url],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            None,
        ));

        if let Some(root) = url_subtree_roots(&strict)
            .as_ref()
            .and_then(|roots| roots.first())
            && strict.url.path() != "/"
        {
            options.push(option(
                "allow_url_subtree",
                "This page and subpages",
                &format!("Allow requested URLs at or below {root}/**."),
                StructuredPermissionEffect::Allow,
                vec![PermissionResourceConstraint {
                    kind: PermissionResourceKind::Url,
                    selector: PermissionResourceSelector::UrlSubtreeDigest {
                        digest: url_subtree_digest(root),
                    },
                    access: resource.access.clone(),
                    protected: Some(false),
                    attributes: BTreeMap::new(),
                }],
                PermissionArgumentConstraint::Unconstrained,
                reusable.clone(),
                true,
                false,
                None,
            ));
        }

        let origin = strict.url.origin().ascii_serialization();
        options.push(option(
            "allow_url_origin",
            "Any page on this origin",
            &format!("Allow any requested URL on {origin}/**."),
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Url,
                selector: PermissionResourceSelector::UrlOriginDigest {
                    digest: url_origin_digest(&resource.value).expect("strict URL has an origin"),
                },
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            None,
        ));
        options.push(option(
            "allow_any_url",
            "Any public HTTP(S) URL",
            "Allow any public HTTP(S) URL accepted by this exact tool contract.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Url,
                selector: PermissionResourceSelector::Any,
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some("ALLOW ANY URL"),
        ));
    }

    if matches!(authority, PermissionAuthorityProfile::Query)
        && let [resource] = resources
        && resource.kind == PermissionResourceKind::Query
    {
        options.push(option(
            "allow_exact_query",
            "This exact search query",
            "Allow this query with different result limits or paging controls.",
            StructuredPermissionEffect::Allow,
            exact_resource_constraints(resources),
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            None,
        ));
        options.push(option(
            "allow_any_query",
            "Any search query",
            "Allow any future query through this exact tool contract.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Query,
                selector: PermissionResourceSelector::Any,
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some("ALLOW ANY SEARCH"),
        ));
    }

    if matches!(authority, PermissionAuthorityProfile::Shell)
        && !resources.is_empty()
        && resources
            .iter()
            .all(|resource| resource.kind == PermissionResourceKind::Command)
    {
        let workdir_label = workdir_label(resources.first());
        // Protected commands were reviewed as whole command lines because analysis
        // dropped operands, so only the blanket options below describe them
        // truthfully.
        if resources.iter().all(|resource| !resource.protected) {
            add_command_options(&mut options, resources, subject, executor, &reusable);
            options.push(option(
                EXACT_COMMANDS_OPTION,
                if resources.len() == 1 {
                    "This command in this workdir"
                } else {
                    "These commands in this workdir"
                },
                &format!(
                    "Allow {} in {workdir_label} with different timeout or display controls.",
                    listed_commands(resources.iter().map(|resource| resource.value.as_str()))
                ),
                StructuredPermissionEffect::Allow,
                exact_resource_constraints(resources),
                PermissionArgumentConstraint::Unconstrained,
                reusable.clone(),
                true,
                false,
                None,
            ));
            let mut patterns = Vec::new();
            let mut exact_fallbacks = Vec::new();
            let pattern_constraints = exact_resource_constraints(resources)
                .into_iter()
                .zip(resources)
                .map(|(mut constraint, resource)| {
                    if let Some(pattern) = super::command_pattern::reusable_prefix(&resource.value)
                    {
                        if !patterns.contains(&pattern) {
                            patterns.push(pattern.clone());
                        }
                        constraint.selector =
                            PermissionResourceSelector::CommandPattern { pattern };
                    } else {
                        exact_fallbacks.push(resource.value.as_str());
                    }
                    constraint
                })
                .collect();
            if !patterns.is_empty() {
                let label = if patterns.len() == 1 && exact_fallbacks.is_empty() {
                    format!(
                        "Any `{}` command in this workdir",
                        patterns[0].strip_suffix(" *").unwrap_or(&patterns[0])
                    )
                } else {
                    "These command patterns in this workdir".into()
                };
                let summaries = patterns
                    .iter()
                    .map(|pattern| safe_summary(pattern))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut description = format!(
                    "Allow commands matching these patterns in {workdir_label}: {summaries}."
                );
                if !exact_fallbacks.is_empty() {
                    description.push_str(&format!(
                        " Also allow {} exactly as reviewed.",
                        listed_commands(exact_fallbacks.iter().copied())
                    ));
                }
                options.push(option(
                    COMMAND_PATTERNS_OPTION,
                    &label,
                    &description,
                    StructuredPermissionEffect::Allow,
                    pattern_constraints,
                    PermissionArgumentConstraint::Unconstrained,
                    reusable.clone(),
                    true,
                    false,
                    None,
                ));
            }
        }
        // One constraint per distinct workdir. Every attribute of a constraint
        // must match for it to cover a resource, so pinning only the first
        // workdir would leave `cd /elsewhere && …` uncoverable and the answer
        // uncommittable.
        let workdirs = resources
            .iter()
            .filter_map(|resource| resource.attributes.get(WORKDIR_ATTRIBUTE))
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if !workdirs.is_empty() {
            options.push(option(
                "allow_commands_in_workdir",
                if workdirs.len() == 1 {
                    "Any command in this workdir"
                } else {
                    "Any command in these workdirs"
                },
                &format!(
                    "Allow arbitrary commands starting in {}.",
                    listed_commands(workdirs.iter().copied())
                ),
                StructuredPermissionEffect::Allow,
                workdirs
                    .iter()
                    .map(|workdir| PermissionResourceConstraint {
                        kind: PermissionResourceKind::Command,
                        selector: PermissionResourceSelector::Any,
                        access: Some(PermissionResourceAccess::Execute),
                        protected: None,
                        attributes: BTreeMap::from([(
                            WORKDIR_ATTRIBUTE.into(),
                            PermissionResourceSelector::Digest {
                                digest: pinned_digest(workdir, &PermissionResourceKind::Directory),
                            },
                        )]),
                    })
                    .collect(),
                PermissionArgumentConstraint::Unconstrained,
                reusable.clone(),
                true,
                false,
                Some(BROAD_SHELL_PHRASE),
            ));
        }
        options.push(option(
            "allow_any_command",
            "Any shell command",
            "Allow arbitrary shell commands from any working directory.",
            StructuredPermissionEffect::Allow,
            vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Command,
                selector: PermissionResourceSelector::Any,
                access: Some(PermissionResourceAccess::Execute),
                protected: None,
                attributes: BTreeMap::new(),
            }],
            PermissionArgumentConstraint::Unconstrained,
            reusable.clone(),
            true,
            false,
            Some(BROAD_SHELL_PHRASE),
        ));
    }

    add_filesystem_options(
        &mut options,
        resources,
        input,
        cwd,
        subject,
        executor,
        &reusable,
        authority,
    );
    if tool.is_mcp() {
        options.push(option(
            "allow_whole_mcp_tool_conversation",
            "Allow whole MCP tool for conversation (broad)",
            "Broad: allow this MCP tool with any arguments for the conversation.",
            StructuredPermissionEffect::Allow,
            resource_constraints,
            PermissionArgumentConstraint::Unconstrained,
            vec![PermissionLifetime::Conversation],
            true,
            false,
            Some("ALLOW MCP TOOL"),
        ));
    }
    options
}

/// How a workdir reads in a sentence, or a stand-in when the resource carries
/// none.
fn workdir_label(resource: Option<&PermissionResource>) -> String {
    resource
        .and_then(|resource| resource.attributes.get(WORKDIR_ATTRIBUTE))
        .map_or_else(
            || "this workdir".to_owned(),
            |workdir| safe_summary(workdir),
        )
}

/// One ladder per command, so a request that batches several can be remembered
/// at a different breadth for each.
///
/// The rungs are deliberately narrow: the command itself, and its reusable
/// prefix when it has one. Anything wider is a claim about the whole request
/// and stays in the blanket options. Rungs leave the arguments unconstrained
/// because a composition cannot pin them for one resource and not another.
fn add_command_options(
    options: &mut Vec<PermissionRuleOption>,
    resources: &[PermissionResource],
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    reusable: &[PermissionLifetime],
) {
    for (index, resource) in resources.iter().enumerate() {
        let constraint = resource_constraint(resource);
        let workdir = workdir_label(Some(resource));
        let command = safe_summary(&resource.value);
        let rung = |id: String,
                    value: &str,
                    description: String,
                    selector: PermissionResourceSelector| PermissionRuleOption {
            id,
            label: command.clone(),
            description,
            rule: StructuredPermissionRule {
                subject: subject.clone(),
                executor: executor.clone(),
                resources: vec![PermissionResourceConstraint {
                    selector,
                    ..constraint.clone()
                }],
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: None,
            },
            allowed_lifetimes: reusable.to_vec(),
            broad: true,
            is_default: false,
            confirmation: None,
            group: Some(PermissionOptionGroup {
                key: format!("{COMMAND_GROUP_PREFIX}{index}"),
                value: value.to_owned(),
                resource: Some(index),
            }),
            caution: None,
        };
        options.push(rung(
            format!("{COMMAND_EXACT_PREFIX}{index}"),
            EXACT_COMMAND_CHIP,
            format!("Allow `{command}` in {workdir} with different timeout or display controls."),
            constraint.selector.clone(),
        ));
        if let Some(pattern) = super::command_pattern::reusable_prefix(&resource.value) {
            options.push(rung(
                format!("{COMMAND_PATTERN_PREFIX}{index}"),
                &pattern,
                format!(
                    "Allow commands matching `{}` in {workdir}.",
                    safe_summary(&pattern)
                ),
                PermissionResourceSelector::CommandPattern {
                    pattern: pattern.clone(),
                },
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn add_filesystem_options(
    options: &mut Vec<PermissionRuleOption>,
    resources: &[PermissionResource],
    input: &Value,
    cwd: &Path,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    reusable: &[PermissionLifetime],
    authority: &PermissionAuthorityProfile,
) {
    if resources.is_empty()
        || resources.iter().any(|resource| {
            !matches!(
                resource.kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            )
        })
    {
        return;
    }
    let PermissionAuthorityProfile::Filesystem { input_pointers } = authority else {
        return;
    };
    let write = resources
        .iter()
        .any(|resource| resource.access == Some(PermissionResourceAccess::Write));
    let search = !input_pointers.is_empty();

    let arguments = if search {
        let Ok(digest) = selected_input_digest(input, input_pointers) else {
            return;
        };
        PermissionArgumentConstraint::SelectedDigest {
            pointers: input_pointers.clone(),
            digest,
        }
    } else {
        PermissionArgumentConstraint::Unconstrained
    };
    options.push(PermissionRuleOption {
        id: "allow_exact_resources".into(),
        label: if resources.len() == 1 {
            "This exact path".into()
        } else {
            "These exact paths".into()
        },
        description: if write {
            "Allow future changes to only the exact reviewed path set.".into()
        } else if search {
            "Allow the same search expression at this exact root.".into()
        } else {
            "Allow future reads of only the exact reviewed path set.".into()
        },
        rule: StructuredPermissionRule {
            subject: subject.clone(),
            executor: executor.clone(),
            resources: exact_resource_constraints(resources),
            arguments,
            lifetime: PermissionLifetime::Once,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        },
        allowed_lifetimes: reusable.to_vec(),
        broad: true,
        is_default: false,
        confirmation: write.then(|| "ALLOW FILE CHANGES".into()),
        group: None,
        caution: None,
    });

    // A protected path never earns a subtree grant, because every rung of the
    // ladder below pins `protected: Some(false)` and so could not cover it
    // anyway. Returning here keeps the exact-path option above, which carries
    // the real protected flag and is reusable across differing tool inputs.
    if resources.iter().any(|resource| resource.protected) {
        return;
    }

    // Only a first-party read earns a widened subtree grant. A write request
    // keeps its exact subject, so `allow_filesystem_subtree` on a write never
    // becomes reachable from a reading contract.
    let family = (is_filesystem_read_subject(subject)
        && resources.iter().all(|resource| {
            is_filesystem_read_kind(&resource.kind)
                && is_filesystem_read_access(resource.access.as_ref())
        }))
    .then_some(PermissionCapabilityFamily::FilesystemRead);

    let mut roots = Vec::with_capacity(resources.len());
    let mut constraints = Vec::with_capacity(resources.len());
    for resource in resources {
        let Some(path) = normalized_filesystem_path(&resource.value) else {
            return;
        };
        let root = if resource.kind == PermissionResourceKind::File {
            path.parent().unwrap_or(&path)
        } else {
            path.as_path()
        };
        roots.push(root.to_path_buf());
        constraints.push(PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: filesystem_subtree_digest(&root.to_string_lossy())
                    .expect("normalized filesystem root has a digest"),
            },
            access: resource.access.clone(),
            protected: Some(false),
            attributes: BTreeMap::new(),
        });
    }
    roots.sort();
    roots.dedup();

    let project = normalized_filesystem_path(&cwd.to_string_lossy());
    let ladder = SubtreeLadder {
        family,
        write,
        project: project.as_deref(),
        repository: enclosing_repository(roots.first().map(PathBuf::as_path)),
        home: caudra_storage::paths::home(),
    };
    options.push(ladder.rung(
        SUBTREE_OPTION_ID.into(),
        &roots,
        constraints,
        subject,
        executor,
        reusable,
    ));

    // Every rung above the first is a single root the whole request fits
    // under, so the ladder walks the ancestors of what the roots have in
    // common. With one root that common ancestor is the root itself, whose own
    // level is already the rung above, so the walk starts one step up.
    let Some(common) = common_ancestor(&roots) else {
        return;
    };
    let climb: Vec<_> = if roots.len() == 1 {
        common.ancestors().skip(1).collect()
    } else {
        common.ancestors().collect()
    };
    for (step, ancestor) in climb.into_iter().enumerate() {
        let Some(digest) = filesystem_subtree_digest(&ancestor.to_string_lossy()) else {
            continue;
        };
        let mut widened: Vec<PermissionResourceConstraint> = Vec::new();
        for resource in resources {
            if widened.iter().any(|constraint| {
                constraint.kind == resource.kind && constraint.access == resource.access
            }) {
                continue;
            }
            widened.push(PermissionResourceConstraint {
                kind: resource.kind.clone(),
                selector: PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: digest.clone(),
                },
                access: resource.access.clone(),
                protected: Some(false),
                attributes: BTreeMap::new(),
            });
        }
        options.push(ladder.rung(
            format!("{SUBTREE_OPTION_ID}_{}", step + 1),
            std::slice::from_ref(&ancestor.to_path_buf()),
            widened,
            subject,
            executor,
            reusable,
        ));
    }
}

/// The context a subtree rung is judged in, so every rung is labelled, described
/// and cautioned by one rule instead of by the loop that happens to build it.
struct SubtreeLadder<'a> {
    family: Option<PermissionCapabilityFamily>,
    write: bool,
    project: Option<&'a Path>,
    repository: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl SubtreeLadder<'_> {
    #[allow(clippy::too_many_arguments)]
    fn rung(
        &self,
        id: String,
        roots: &[PathBuf],
        resources: Vec<PermissionResourceConstraint>,
        subject: &PermissionSubject,
        executor: &PermissionExecutorKind,
        reusable: &[PermissionLifetime],
    ) -> PermissionRuleOption {
        let patterns = roots
            .iter()
            .map(|root| format!("{}/**", root.to_string_lossy().trim_end_matches('/')))
            .collect::<Vec<_>>()
            .join(", ");
        let caution = self.caution(roots);
        let mut value = patterns.clone();
        if roots.len() == 1 && self.project == Some(roots[0].as_path()) {
            value.push_str(" (project root)");
        }
        let mut description = if self.family.is_some() {
            format!("Allow reading, listing, and searching any path below {patterns}.")
        } else {
            format!("Allow matching paths below {patterns}.")
        };
        if let Some(reason) = self.reason(caution) {
            description.push(' ');
            description.push_str(&reason);
        }
        PermissionRuleOption {
            id,
            label: "These directories and descendants".into(),
            description,
            rule: StructuredPermissionRule {
                subject: subject.clone(),
                executor: executor.clone(),
                resources,
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: PermissionLifetime::Once,
                effect: StructuredPermissionEffect::Allow,
                family: self.family,
            },
            allowed_lifetimes: reusable.to_vec(),
            broad: true,
            is_default: false,
            confirmation: self.confirmation(caution),
            group: Some(PermissionOptionGroup {
                key: SUBTREE_OPTION_ID.into(),
                value,
                resource: None,
            }),
            caution,
        }
    }

    /// How much a rung exposes, measured against the two landmarks a user
    /// reasons about. Grave once the grant swallows the home directory, which a
    /// root outside home entirely does not do — that root is beside home, not
    /// above it, and reaches nothing home holds. An unknown home is grave,
    /// because a boundary that cannot be found cannot be respected.
    fn caution(&self, roots: &[PathBuf]) -> Option<PermissionCaution> {
        let swallows_home = self.home.as_deref().is_none_or(|home| {
            roots
                .iter()
                .any(|root| home == root || home.starts_with(root))
        });
        if swallows_home {
            return Some(PermissionCaution::Danger);
        }
        let outside_repository = self.repository.as_deref().is_none_or(|repository| {
            roots
                .iter()
                .any(|root| root != repository && !root.starts_with(repository))
        });
        outside_repository.then_some(PermissionCaution::Warn)
    }

    fn reason(&self, caution: Option<PermissionCaution>) -> Option<String> {
        match caution? {
            PermissionCaution::Danger => Some("This takes in your whole home directory.".into()),
            PermissionCaution::Warn => Some(match &self.repository {
                Some(repository) => format!(
                    "This reaches outside the repository at {}.",
                    repository.display()
                ),
                None => "This reaches outside any repository.".into(),
            }),
        }
    }

    /// The phrase to type before the grant is stored. Reaching past home is the
    /// graver claim, so it names itself even when the grant also writes.
    fn confirmation(&self, caution: Option<PermissionCaution>) -> Option<String> {
        if caution == Some(PermissionCaution::Danger) {
            return Some(OUTSIDE_HOME_PHRASE.into());
        }
        self.write.then(|| "ALLOW DIRECTORY CHANGES".into())
    }
}

/// The deepest directory every root sits under, which is the first rung the
/// whole request can share.
fn common_ancestor(roots: &[PathBuf]) -> Option<PathBuf> {
    let mut shared = roots.first()?.clone();
    for root in &roots[1..] {
        while !root.starts_with(&shared) {
            if !shared.pop() {
                return Some(shared);
            }
        }
    }
    Some(shared)
}

/// The nearest ancestor holding a `.git` entry, which is what "outside the
/// repository" is measured against. A worktree or submodule records a file
/// rather than a directory, so existence is the test.
fn enclosing_repository(start: Option<&Path>) -> Option<PathBuf> {
    start?
        .ancestors()
        .find(|ancestor| ancestor.join(GIT_METADATA_DIR).exists())
        .map(Path::to_path_buf)
}

fn presentation_for(
    tool: &ToolKey,
    risk: &PermissionRisk,
    resources: &[PermissionResource],
) -> PermissionPresentation {
    let action = match tool {
        ToolKey::McpTool { server, tool } => {
            format!(
                "Call MCP tool {} from {}",
                safe_summary(tool),
                safe_summary(server)
            )
        }
        ToolKey::McpServer { server } => {
            format!("Call an MCP tool from {}", safe_summary(server))
        }
        ToolKey::Native(name) => format!("Run native tool {}", safe_summary(name)),
        ToolKey::Wildcard => "Run an unknown legacy tool".into(),
    };
    let risk_summary = match risk {
        PermissionRisk::Low => "Limited read or lookup operation",
        PermissionRisk::Medium => "External read or network operation",
        PermissionRisk::High => "May modify data, execute code, or invoke an external authority",
        PermissionRisk::Critical => "Complex or protected operation requiring exact review",
        PermissionRisk::Unknown => "Legacy operation with unknown effects",
    }
    .into();
    PermissionPresentation {
        action,
        risk: risk.clone(),
        risk_summary,
        resources: resources
            .iter()
            .map(|resource| {
                let mut summary = if resource.kind == PermissionResourceKind::Url {
                    redacted_url_summary(&resource.value)
                } else {
                    safe_summary(&resource.value)
                };
                if let Some(workdir) = resource.attributes.get("workdir") {
                    summary.push_str(" in ");
                    summary.push_str(&safe_summary(workdir));
                }
                PermissionResourcePresentation {
                    kind: resource.kind.clone(),
                    access: resource.access.clone(),
                    summary,
                    protected: resource.protected,
                    coverage: None,
                }
            })
            .collect(),
    }
}

pub fn update_presentation_coverage(
    presentation: &mut PermissionPresentation,
    coverage: &[Option<ResourceCoverage>],
) -> bool {
    if presentation.resources.len() != coverage.len() {
        return false;
    }
    for (resource, coverage) in presentation.resources.iter_mut().zip(coverage) {
        resource.coverage.clone_from(coverage);
    }
    true
}

fn redacted_url_summary(value: &str) -> String {
    let Some(strict) = strict_http_url(value) else {
        return safe_summary(value);
    };
    let mut url = strict.url;
    if url.query().is_some() {
        let query = url
            .query_pairs()
            .map(|(key, _)| format!("{key}=<redacted>"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&query));
    }
    safe_summary(url.as_str())
}

fn safe_summary(value: &str) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in value.chars().enumerate() {
        if index == SUMMARY_MAX_CHARS {
            truncated = true;
            break;
        }
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if truncated {
        output.push_str("...");
    }
    output
}

/// Renders reviewed commands as a backtick-quoted list, capped for readability.
fn listed_commands<'a>(commands: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = commands.len();
    let listed = commands
        .take(LISTED_COMMANDS_MAX)
        .map(|command| format!("`{}`", safe_summary(command)))
        .collect::<Vec<_>>()
        .join(", ");
    if total > LISTED_COMMANDS_MAX {
        format!("{listed}, +{} more", total - LISTED_COMMANDS_MAX)
    } else {
        listed
    }
}

fn deserialize_tool_key<'de, D>(deserializer: D) -> Result<ToolKey, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    ToolKey::parse(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::tools::PermissionScopes;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    fn request(resources: Vec<PermissionResource>) -> PermissionRequest {
        PermissionRequest::from_legacy(
            "request".into(),
            ToolKey::native("legacy"),
            resources
                .iter()
                .map(|resource| resource.value.clone())
                .collect(),
            json!({"branch": "main", "nested": {"value": 1}}),
            Path::new("/tmp"),
            false,
        )
        .with_resources(resources)
    }

    trait RequestTestExt {
        fn with_resources(self, resources: Vec<PermissionResource>) -> Self;
    }

    impl RequestTestExt for PermissionRequest {
        fn with_resources(mut self, resources: Vec<PermissionResource>) -> Self {
            self.resources = resources;
            self
        }
    }

    fn custom_resource(value: &str) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: "test".into(),
            },
            value: value.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }
    }

    fn command_resource(value: &str, workdir: &str) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::Command,
            value: value.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::from([("workdir".into(), workdir.into())]),
        }
    }

    fn protected_command_resource(value: &str, workdir: &str) -> PermissionResource {
        PermissionResource {
            protected: true,
            requires_prompt: true,
            ..command_resource(value, workdir)
        }
    }

    fn explicit_request(
        authority: PermissionAuthorityProfile,
        resources: Vec<PermissionResource>,
        input: Value,
    ) -> PermissionRequest {
        let intent = PermissionIntent::new(
            PermissionScopes::single("legacy-scope".into()),
            resources,
            PermissionRisk::Medium,
        )
        .with_authority(authority);
        PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("generic_platform_tool"),
            &intent,
            input,
            Path::new("/project"),
            PermissionSubject::Native {
                owner: "first-party".into(),
                contract: "platform/v1".into(),
            },
            PermissionExecutorKind::Native,
        )
    }

    fn exact_constraint(resource: &PermissionResource) -> PermissionResourceConstraint {
        PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Exact {
                value: resource.value.clone(),
            },
            access: resource.access.clone(),
            protected: Some(resource.protected),
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn explicit_authority_profiles_do_not_depend_on_tool_names() {
        let filesystem = explicit_request(
            PermissionAuthorityProfile::Filesystem {
                input_pointers: vec!["/pattern".into()],
            },
            vec![PermissionResource {
                kind: PermissionResourceKind::Directory,
                value: "/project/src".into(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"pattern": "needle", "limit": 10}),
        );
        let url = explicit_request(
            PermissionAuthorityProfile::Url,
            vec![PermissionResource {
                kind: PermissionResourceKind::Url,
                value: "https://example.com/docs/page".into(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"url": "https://example.com/docs/page"}),
        );
        let query = explicit_request(
            PermissionAuthorityProfile::Query,
            vec![PermissionResource {
                kind: PermissionResourceKind::Query,
                value: "rust permissions".into(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"query": "rust permissions"}),
        );
        let shell = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![PermissionResource {
                kind: PermissionResourceKind::Command,
                value: "cargo test".into(),
                access: Some(PermissionResourceAccess::Execute),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("workdir".into(), "/project".into())]),
            }],
            json!({"command": "cargo test", "timeout": 30}),
        );
        let exact = explicit_request(
            PermissionAuthorityProfile::ExactOnly,
            vec![custom_resource("opaque")],
            json!({"value": "opaque"}),
        );

        for (request, option) in [
            (&filesystem, "allow_exact_resources"),
            (&url, "allow_exact_url"),
            (&query, "allow_exact_query"),
            (&shell, "allow_exact_commands"),
        ] {
            assert!(
                request
                    .options
                    .iter()
                    .any(|candidate| candidate.id == option)
            );
            assert_eq!(request.risk, PermissionRisk::Medium);
        }
        assert_eq!(
            exact
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["allow_exact", "deny_exact"]
        );
    }

    #[test]
    fn native_contract_identity_is_strict_for_explicit_intents() {
        let request = explicit_request(
            PermissionAuthorityProfile::ExactOnly,
            vec![custom_resource("opaque")],
            json!({"value": "opaque"}),
        );
        let rule = request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        let mut other_contract = request.clone();
        other_contract.subject = PermissionSubject::Native {
            owner: "first-party".into(),
            contract: "platform/v2".into(),
        };

        assert!(permission_rule_covers_request(&rule, &request));
        assert!(!permission_rule_covers_request(&rule, &other_contract));
    }

    fn rule(
        request: &PermissionRequest,
        effect: StructuredPermissionEffect,
        resources: Vec<PermissionResourceConstraint>,
    ) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: request.subject.clone(),
            executor: request.executor.clone(),
            resources,
            arguments: PermissionArgumentConstraint::Exact {
                digest: request.input_digest.clone(),
            },
            lifetime: request.lifetime.clone(),
            effect,
            family: None,
        }
    }

    #[test]
    fn canonical_digest_is_stable_across_object_key_order() {
        let left = json!({"z": 1, "a": {"d": 4, "b": 2}, "items": [3, 2, 1]});
        let right = json!({"items": [3, 2, 1], "a": {"b": 2, "d": 4}, "z": 1});
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_json_sha256(&left), canonical_json_sha256(&right));
        assert_eq!(
            canonical_json(&left),
            r#"{"a":{"b":2,"d":4},"items":[3,2,1],"z":1}"#
        );
    }

    #[test]
    fn bash_resource_uses_the_framed_execution_workdir() {
        let workdir = "/tmp/actual";
        let scope = format!(
            "cargo test # caudra-workdir[{}]={workdir} # caudra-frame[{}]",
            workdir.len(),
            workdir.len()
        );
        let request = PermissionRequest::from_legacy(
            "request".into(),
            ToolKey::native("bash"),
            vec![scope],
            json!({"command": "cd /tmp/actual && cargo test"}),
            Path::new("/tmp/project"),
            false,
        );

        assert_eq!(request.resources[0].value, "cargo test");
        assert_eq!(
            request.resources[0]
                .attributes
                .get("workdir")
                .map(String::as_str),
            Some(workdir)
        );
    }

    #[test]
    fn persisted_review_shape_never_contains_argument_values() {
        let input = json!({
            "command": "deploy --token secret",
            "force": true,
            "retries": 3,
            "targets": ["production"]
        });

        let review = redacted_review_shape(&input);
        assert_eq!(
            review,
            json!({
                "<field:1>": "<string:21 chars>",
                "<field:2>": "<boolean>",
                "<field:3>": "<number>",
                "<field:4>": ["<string:10 chars>"]
            })
        );
        let persisted = serde_json::to_string(&review).unwrap();
        assert!(!persisted.contains("secret"));
        assert!(!persisted.contains("production"));
    }

    #[test]
    fn selected_input_uses_safe_pointer_boundaries() {
        let input = json!({"a/b": {"~key": ["zero", "one"]}, "a": {"b": "other"}});
        let pointer = json_pointer(&["a/b", "~key", "1"]);
        assert_eq!(pointer, "/a~1b/~0key/1");
        assert_eq!(selected_input_pointer(&input, &pointer).unwrap(), "one");
        assert!(matches!(
            selected_input_pointer(&input, "/a~1b/~0key/01"),
            Err(SelectedInputError::InvalidArrayIndex(_))
        ));
        assert!(matches!(
            selected_input_pointer(&input, "/a~2b"),
            Err(SelectedInputError::InvalidEscape(_))
        ));
        assert!(matches!(
            selected_input(&input, &[pointer.as_str(), pointer.as_str()]),
            Err(SelectedInputError::DuplicatePointer(_))
        ));
    }

    #[test]
    fn selected_and_exact_argument_constraints_match_canonically() {
        let input = json!({"ignored": 1, "selected": {"b": 2, "a": 1}});
        let selected = selected_input(&input, &["/selected"]).unwrap();
        let selected_constraint = PermissionArgumentConstraint::Selected {
            arguments: selected,
        };
        assert!(argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 2}, "ignored": 99})
        ));
        assert!(!argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 3}, "ignored": 1})
        ));
        let exact = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&input),
        };
        assert!(argument_constraint_matches(&exact, &input));
        assert!(!argument_constraint_matches(
            &exact,
            &json!({"ignored": 2, "selected": {"b": 2, "a": 1}})
        ));
    }

    #[test]
    fn filesystem_subtree_uses_component_boundaries() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::File,
            selector: PermissionResourceSelector::Subtree {
                root: root.to_string_lossy().into(),
            },
            access: Some(PermissionResourceAccess::Write),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = |path: PathBuf| PermissionResource {
            kind: PermissionResourceKind::File,
            value: path.to_string_lossy().into(),
            access: Some(PermissionResourceAccess::Write),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(
            &constraint,
            &resource(root.join("src/lib.rs"))
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource(temp.path().join("project-copy/src/lib.rs"))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn filesystem_subtree_resolves_symlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::File,
            selector: PermissionResourceSelector::Subtree {
                root: root.to_string_lossy().into(),
            },
            access: None,
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = PermissionResource {
            kind: PermissionResourceKind::File,
            value: link.join("new.txt").to_string_lossy().into(),
            access: None,
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(&constraint, &resource));
    }

    #[test]
    fn url_subtree_requires_strict_origin_and_path_boundary() {
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::Url,
            selector: PermissionResourceSelector::Subtree {
                root: "https://example.com/api".into(),
            },
            access: Some(PermissionResourceAccess::Read),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = |value: &str| PermissionResource {
            kind: PermissionResourceKind::Url,
            value: value.into(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(
            &constraint,
            &resource("https://example.com/api/v1?q=ok")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://example.com/apiv1")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://user@example.com/api/v1")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://example.com/api/%2e%2e/admin")
        ));
        assert!(resource_constraint_matches(
            &constraint,
            &resource("http://example.com/api/v1")
        ));
    }

    #[test]
    fn separate_allow_rules_union_to_cover_a_multi_command_request() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second.clone()]);
        let allow_rules = vec![
            rule(
                &request,
                StructuredPermissionEffect::Allow,
                vec![exact_constraint(&first)],
            ),
            rule(
                &request,
                StructuredPermissionEffect::Allow,
                vec![exact_constraint(&second)],
            ),
        ];

        assert!(permission_rule_covers_resource(
            &allow_rules[0],
            &request,
            &first
        ));
        assert!(permission_rule_covers_resource(
            &allow_rules[1],
            &request,
            &second
        ));
        assert!(permission_rules_cover_request(&allow_rules, &request));
        assert_eq!(
            evaluate_structured_permission_rules(&allow_rules, &request),
            StructuredPermissionDecision::Allow
        );
        assert!(!permission_rule_covers_request(&allow_rules[0], &request));
    }

    const ALLOW: StructuredPermissionEffect = StructuredPermissionEffect::Allow;
    const ASK: StructuredPermissionEffect = StructuredPermissionEffect::Ask;
    const DENY: StructuredPermissionEffect = StructuredPermissionEffect::Deny;

    fn decision_over(effects: &[StructuredPermissionEffect]) -> StructuredPermissionDecision {
        let resource = command_resource("cargo test", "/project");
        let request = request(vec![resource.clone()]);
        let rules: Vec<_> = effects
            .iter()
            .map(|effect| rule(&request, effect.clone(), vec![exact_constraint(&resource)]))
            .collect();

        resource_decision(&rules, &request, &resource)
    }

    /// The precedence is the whole contract of a rule set: authority only ever
    /// narrows, so the strictest rule that matches decides, whatever order the
    /// set is stored in.
    #[test_case(&[] => StructuredPermissionDecision::NoMatch ; "silence")]
    #[test_case(&[ALLOW] => StructuredPermissionDecision::Allow ; "a_lone_allow")]
    #[test_case(&[ASK] => StructuredPermissionDecision::Ask ; "a_lone_ask")]
    #[test_case(&[DENY] => StructuredPermissionDecision::Deny ; "a_lone_deny")]
    #[test_case(&[ALLOW, ASK] => StructuredPermissionDecision::Ask ; "ask_outranks_allow")]
    #[test_case(&[ASK, ALLOW] => StructuredPermissionDecision::Ask ; "ask_outranks_allow_reversed")]
    #[test_case(&[ALLOW, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_allow")]
    #[test_case(&[DENY, ALLOW] => StructuredPermissionDecision::Deny ; "deny_outranks_allow_reversed")]
    #[test_case(&[ASK, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_ask")]
    #[test_case(&[ALLOW, ASK, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_everything")]
    fn a_rule_set_is_decided_by_its_strictest_match(
        effects: &[StructuredPermissionEffect],
    ) -> StructuredPermissionDecision {
        decision_over(effects)
    }

    /// A rule naming no resource is unrestricted, so it reaches resources no
    /// constraint mentions. Deny already read it that way; allow and ask now
    /// agree, which is what lets one traversal serve all three.
    #[test_case(ALLOW => StructuredPermissionDecision::Allow ; "unrestricted_allow")]
    #[test_case(ASK => StructuredPermissionDecision::Ask ; "unrestricted_ask")]
    #[test_case(DENY => StructuredPermissionDecision::Deny ; "unrestricted_deny")]
    fn an_unconstrained_rule_reaches_every_resource(
        effect: StructuredPermissionEffect,
    ) -> StructuredPermissionDecision {
        let resource = command_resource("cargo test", "/project");
        let request = request(vec![resource.clone()]);
        let unrestricted = rule(&request, effect, Vec::new());

        resource_decision(&[unrestricted], &request, &resource)
    }

    /// Narrowing one resource narrows the call, but authorizing one does not
    /// authorize the call.
    #[test]
    fn a_request_is_denied_by_one_resource_and_allowed_only_by_all() {
        let allowed = command_resource("cargo test", "/project");
        let other = command_resource("git status", "/project");
        let request = request(vec![allowed.clone(), other.clone()]);
        let allow_one = rule(&request, ALLOW, vec![exact_constraint(&allowed)]);
        let allow_other = rule(&request, ALLOW, vec![exact_constraint(&other)]);
        let deny_one = rule(&request, DENY, vec![exact_constraint(&allowed)]);
        let ask_one = rule(&request, ASK, vec![exact_constraint(&allowed)]);

        assert_eq!(
            evaluate_structured_permission_rules(std::slice::from_ref(&allow_one), &request),
            StructuredPermissionDecision::NoMatch
        );
        assert_eq!(
            evaluate_structured_permission_rules(
                &[allow_one.clone(), allow_other.clone()],
                &request
            ),
            StructuredPermissionDecision::Allow
        );
        assert_eq!(
            evaluate_structured_permission_rules(
                &[allow_one.clone(), allow_other.clone(), ask_one],
                &request
            ),
            StructuredPermissionDecision::Ask
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[allow_one, allow_other, deny_one], &request),
            StructuredPermissionDecision::Deny
        );
    }

    const BROAD_ASK: &str = "git *";
    const NARROW_ALLOW: &str = "git status *";
    const WILDCARD_ONLY: &str = "*";
    const SUBTREE_DIGEST: &str = "subtree";
    const NARROW_COMMAND: &str = "git status --short";
    const BROAD_COMMAND: &str = "git commit -m message";

    fn pattern_constraint(pattern: &str) -> PermissionResourceConstraint {
        PermissionResourceConstraint {
            kind: PermissionResourceKind::Command,
            selector: PermissionResourceSelector::CommandPattern {
                pattern: pattern.into(),
            },
            access: None,
            protected: None,
            attributes: BTreeMap::new(),
        }
    }

    fn any_command_constraint() -> PermissionResourceConstraint {
        PermissionResourceConstraint {
            selector: PermissionResourceSelector::Any,
            ..pattern_constraint(BROAD_ASK)
        }
    }

    /// Answers for one command, and proves the answer is the rule set's rather
    /// than the emission order's by requiring the reversed set to agree.
    fn order_independent_command_decision(
        command: &str,
        rules: impl Fn(&PermissionRequest) -> Vec<StructuredPermissionRule>,
    ) -> StructuredPermissionDecision {
        let resource = command_resource(command, "/project");
        let request = request(vec![resource.clone()]);
        let mut reversed = rules(&request);
        reversed.reverse();

        let decision = resource_decision(&rules(&request), &request, &resource);
        assert_eq!(
            resource_decision(&reversed, &request, &resource),
            decision,
            "reversing the rules changed the decision"
        );
        decision
    }

    /// A config that asks about a family and allows one member of it means the
    /// allow to win, so the rules have to be ranked rather than folded on the
    /// effect alone.
    #[test_case(NARROW_COMMAND => StructuredPermissionDecision::Allow ; "the narrower allow wins where it applies")]
    #[test_case(BROAD_COMMAND => StructuredPermissionDecision::Ask ; "the broader ask still covers everything else")]
    fn a_narrower_command_pattern_outranks_a_broader_one(
        command: &str,
    ) -> StructuredPermissionDecision {
        order_independent_command_decision(command, |request| {
            vec![
                rule(request, ASK, vec![pattern_constraint(BROAD_ASK)]),
                rule(request, ALLOW, vec![pattern_constraint(NARROW_ALLOW)]),
            ]
        })
    }

    /// Equal width leaves nothing to rank on, so the safer effect decides.
    #[test_case(pattern_constraint(NARROW_ALLOW), pattern_constraint(NARROW_ALLOW) ; "two rules naming the same pattern")]
    #[test_case(pattern_constraint(WILDCARD_ONLY), any_command_constraint() ; "a bare wildcard names no more than a blanket selector")]
    fn equally_wide_rules_break_the_tie_toward_asking(
        allow: PermissionResourceConstraint,
        ask: PermissionResourceConstraint,
    ) {
        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(request, ALLOW, vec![allow.clone()]),
                rule(request, ASK, vec![ask.clone()]),
            ]),
            StructuredPermissionDecision::Ask
        );
    }

    /// Width ranks what a rule set permits, never what it refuses, so a denial
    /// cannot be out-specified.
    #[test]
    fn a_broad_deny_outranks_an_exact_allow() {
        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(
                    request,
                    ALLOW,
                    vec![exact_constraint(&command_resource(
                        NARROW_COMMAND,
                        "/project"
                    ))]
                ),
                rule(request, DENY, vec![pattern_constraint(BROAD_ASK)]),
            ]),
            StructuredPermissionDecision::Deny
        );
    }

    /// A grant naming one exact resource is the narrowest statement there is, so
    /// saving "always allow this" has to survive a broad ask.
    #[test_case(pattern_constraint(BROAD_ASK) => StructuredPermissionDecision::Allow ; "outranks a pattern")]
    #[test_case(any_command_constraint() => StructuredPermissionDecision::Allow ; "outranks a blanket selector")]
    #[test_case(exact_constraint(&command_resource(NARROW_COMMAND, "/project")) => StructuredPermissionDecision::Ask ; "ties with another exact and yields")]
    fn an_exact_grant_outranks_a_wider_ask(
        ask: PermissionResourceConstraint,
    ) -> StructuredPermissionDecision {
        order_independent_command_decision(NARROW_COMMAND, |request| {
            vec![
                rule(request, ASK, vec![ask.clone()]),
                rule(
                    request,
                    ALLOW,
                    vec![exact_constraint(&command_resource(
                        NARROW_COMMAND,
                        "/project",
                    ))],
                ),
            ]
        })
    }

    /// Protection raises the bar for granting, so a rule naming a pattern cannot
    /// grant a protected command. It must not raise the same bar for refusing, or
    /// a deny would fail open on exactly the commands protection exists for.
    #[test_case(ALLOW => StructuredPermissionDecision::NoMatch ; "a pattern cannot grant it")]
    #[test_case(ASK => StructuredPermissionDecision::Ask ; "a pattern can still ask about it")]
    #[test_case(DENY => StructuredPermissionDecision::Deny ; "a pattern can still refuse it")]
    fn protection_gates_grants_and_not_refusals(
        effect: StructuredPermissionEffect,
    ) -> StructuredPermissionDecision {
        let resource = protected_command_resource(NARROW_COMMAND, "/project");
        let request = request(vec![resource.clone()]);
        let rules = vec![rule(&request, effect, vec![pattern_constraint(BROAD_ASK)])];

        resource_decision(&rules, &request, &resource)
    }

    /// The ranking table. The variant order supplies the comparison; this pins
    /// what each selector is worth, which is where a selector could silently
    /// rank as wider or narrower than it reaches.
    #[test_case(PermissionResourceSelector::Any => SelectorWidth::Blanket ; "a blanket selector names everything")]
    #[test_case(PermissionResourceSelector::CommandPattern { pattern: WILDCARD_ONLY.into() } => SelectorWidth::Blanket ; "a pattern with no literal names everything too")]
    #[test_case(PermissionResourceSelector::CommandPattern { pattern: BROAD_ASK.into() } => SelectorWidth::Region(1, 3) ; "a pattern names its literals")]
    #[test_case(PermissionResourceSelector::FilesystemSubtreeDigest { digest: SUBTREE_DIGEST.into() } => SelectorWidth::Region(0, 0) ; "a subtree names a region")]
    #[test_case(PermissionResourceSelector::Exact { value: NARROW_COMMAND.into() } => SelectorWidth::Exact ; "an exact selector names one resource")]
    #[test_case(PermissionResourceSelector::Prefix { value: "git status".into() } => SelectorWidth::Region(2, 9) ; "a prefix is measured like the pattern it competes with")]
    #[test_case(PermissionResourceSelector::Prefix { value: String::new() } => SelectorWidth::Blanket ; "an empty prefix names everything")]
    fn selector_width_reflects_how_much_a_selector_names(
        selector: PermissionResourceSelector,
    ) -> SelectorWidth {
        selector_width(&selector)
    }

    /// The one configured form the structured model had no equivalent for: a
    /// scope ending in a bare `*`. It is raw text matched raw, on any kind, so
    /// that the deny rules already written against it keep matching.
    #[test_case(PermissionResourceKind::Command, "git status --short" => true ; "reaches a command it prefixes")]
    #[test_case(PermissionResourceKind::Command, "git stash" => false ; "stops where the prefix stops")]
    #[test_case(PermissionResourceKind::Command, "sudo git status" => false ; "must start the value, not merely appear in it")]
    #[test_case(PermissionResourceKind::File, "git status --short" => true ; "is not tied to one kind")]
    fn a_prefix_selector_reaches_what_it_starts(kind: PermissionResourceKind, value: &str) -> bool {
        selector_matches(
            &PermissionResourceSelector::Prefix {
                value: "git stat".into(),
            },
            value,
            &kind,
        )
    }

    /// A configured scope becomes a prefix or a command pattern purely by its
    /// spelling, so the two have to rank against each other rather than by which
    /// kind of selector they became.
    #[test]
    fn a_prefix_outranks_a_command_pattern_that_pins_less() {
        let prefix = PermissionResourceConstraint {
            selector: PermissionResourceSelector::Prefix {
                value: "git status".into(),
            },
            ..pattern_constraint(BROAD_ASK)
        };

        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(request, ASK, vec![pattern_constraint(BROAD_ASK)]),
                rule(request, ALLOW, vec![prefix.clone()]),
            ]),
            StructuredPermissionDecision::Allow
        );
    }

    const MCP_SERVER: &str = "deepwiki";
    const OTHER_SERVER: &str = "othersrv";
    const MINTED_TOOL: &str = "search";
    const OTHER_TOOL: &str = "fetch";

    fn mcp_subject(server: &str, tool: &str) -> PermissionSubject {
        PermissionSubject::Mcp {
            server: server.into(),
            authority: server.into(),
            tool: tool.into(),
            contract: MCP_CONTRACT.into(),
        }
    }

    /// A server-wide rule is keyed to the server, so the tool that happened to
    /// ask first stops being part of the key. It must not reach another server,
    /// and without the family it must not reach another tool either.
    #[test_case(Some(PermissionCapabilityFamily::McpServer), MCP_SERVER, OTHER_TOOL => true ; "reaches_a_sibling_tool")]
    #[test_case(Some(PermissionCapabilityFamily::McpServer), MCP_SERVER, MINTED_TOOL => true ; "still_reaches_its_own_tool")]
    #[test_case(Some(PermissionCapabilityFamily::McpServer), OTHER_SERVER, MINTED_TOOL => false ; "never_crosses_to_another_server")]
    #[test_case(None, MCP_SERVER, OTHER_TOOL => false ; "without_the_family_one_tool_stays_one_tool")]
    fn an_mcp_server_family_widens_to_the_server_and_no_further(
        family: Option<PermissionCapabilityFamily>,
        request_server: &str,
        request_tool: &str,
    ) -> bool {
        let resource = command_resource("query", "/project");
        let mut minted = request(vec![resource.clone()]);
        minted.subject = mcp_subject(MCP_SERVER, MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&resource)]);
        rule.family = family;

        let mut incoming = request(vec![resource.clone()]);
        incoming.subject = mcp_subject(request_server, request_tool);

        resource_decision(&[rule], &incoming, &resource) == StructuredPermissionDecision::Allow
    }

    /// A record written before servers were recorded deserializes with an empty
    /// server, which must not become a key that reaches every server.
    #[test]
    fn an_unnamed_mcp_server_never_matches() {
        let resource = command_resource("query", "/project");
        let mut minted = request(vec![resource.clone()]);
        minted.subject = mcp_subject("", MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&resource)]);
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        let mut incoming = request(vec![resource.clone()]);
        incoming.subject = mcp_subject("", OTHER_TOOL);

        assert_eq!(
            resource_decision(&[rule], &incoming, &resource),
            StructuredPermissionDecision::NoMatch
        );
    }

    /// Widening the subject must not widen the operation, or a rule minted from
    /// a read would reach a write on a sibling tool.
    #[test]
    fn an_mcp_server_family_does_not_widen_the_operation() {
        let read = PermissionResource {
            kind: PermissionResourceKind::File,
            value: "/project/notes.md".into(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        };
        let write = PermissionResource {
            access: Some(PermissionResourceAccess::Write),
            ..read.clone()
        };
        let mut minted = request(vec![read.clone()]);
        minted.subject = mcp_subject(MCP_SERVER, MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&read)]);
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        let mut incoming = request(vec![write.clone()]);
        incoming.subject = mcp_subject(MCP_SERVER, OTHER_TOOL);

        assert_eq!(
            resource_decision(std::slice::from_ref(&rule), &incoming, &write),
            StructuredPermissionDecision::NoMatch
        );
        let mut same_tool_read = request(vec![read.clone()]);
        same_tool_read.subject = mcp_subject(MCP_SERVER, OTHER_TOOL);
        assert_eq!(
            resource_decision(&[rule], &same_tool_read, &read),
            StructuredPermissionDecision::Allow
        );
    }

    #[test]
    fn missing_resource_keeps_union_coverage_at_no_match() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second]);
        let allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );

        assert!(!permission_rules_cover_request(
            std::slice::from_ref(&allow),
            &request
        ));
        assert_eq!(
            evaluate_structured_permission_rules(&[allow], &request),
            StructuredPermissionDecision::NoMatch
        );
    }

    #[test]
    fn exact_input_rule_cannot_contribute_to_a_different_input() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second.clone()]);
        let mut wrong_input = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );
        wrong_input.arguments = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&json!({"branch": "other"})),
        };
        let second_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&second)],
        );

        assert!(!permission_rule_covers_resource(
            &wrong_input,
            &request,
            &first
        ));
        assert_eq!(
            evaluate_structured_permission_rules(&[wrong_input, second_allow], &request),
            StructuredPermissionDecision::NoMatch
        );
    }

    #[test]
    fn deny_intersection_blocks_if_any_resource_matches() {
        let first = custom_resource("first");
        let second = custom_resource("second");
        let request = request(vec![first.clone(), second.clone()]);
        let first_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );
        let second_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&second)],
        );
        let deny = rule(
            &request,
            StructuredPermissionEffect::Deny,
            vec![exact_constraint(&second)],
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[first_allow, second_allow, deny], &request),
            StructuredPermissionDecision::Deny
        );
    }

    #[test]
    fn context_matching_is_strict_for_subject_and_executor_not_lifetime() {
        let resource = custom_resource("resource");
        let request = request(vec![resource.clone()]);
        let base = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&resource)],
        );
        let mut wrong_subject = base.clone();
        wrong_subject.subject = PermissionSubject::UnknownLegacy {
            identity: "other".into(),
        };
        let mut wrong_executor = base.clone();
        wrong_executor.executor = PermissionExecutorKind::Mcp;
        let mut wrong_lifetime = base;
        wrong_lifetime.lifetime = PermissionLifetime::Conversation;
        assert!(!permission_rule_covers_request(&wrong_subject, &request));
        assert!(!permission_rule_covers_request(&wrong_executor, &request));
        assert!(permission_rule_covers_request(&wrong_lifetime, &request));
    }

    #[test]
    fn protected_resources_require_an_explicit_exact_selector() {
        let mut resource = custom_resource("secret");
        resource.protected = true;
        let broad = PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Any,
            access: resource.access.clone(),
            protected: Some(true),
            attributes: BTreeMap::new(),
        };
        assert!(!resource_constraint_matches(&broad, &resource));
        assert!(resource_constraint_matches(
            &exact_constraint(&resource),
            &resource
        ));
    }

    #[test]
    fn protected_command_does_not_treat_a_command_pattern_as_exact() {
        let mut resource = command_resource("git diff --stat", "/project");
        resource.protected = true;
        let mut pattern = exact_resource_constraints(std::slice::from_ref(&resource))
            .pop()
            .unwrap();
        pattern.selector = PermissionResourceSelector::CommandPattern {
            pattern: "git diff *".into(),
        };

        assert!(!resource_constraint_matches(&pattern, &resource));
    }

    #[test]
    fn command_pattern_matches_quoted_command_tokens_only_for_commands() {
        let selector = PermissionResourceSelector::CommandPattern {
            pattern: "git diff *".into(),
        };

        assert!(selector_matches(
            &selector,
            r#"git "diff" -- "src/file name.rs""#,
            &PermissionResourceKind::Command
        ));
        assert!(!selector_matches(
            &selector,
            r#"git "diff" -- "src/file name.rs""#,
            &PermissionResourceKind::Query
        ));
    }

    #[test]
    fn mcp_options_keep_broad_choice_non_default() {
        let request = PermissionRequest::from_legacy(
            "mcp".into(),
            ToolKey::McpTool {
                server: Arc::from("server"),
                tool: Arc::from("lookup"),
            },
            vec![canonical_json(&json!({"query": "value"}))],
            json!({"query": "value"}),
            Path::new("/tmp"),
            false,
        );
        let broad = request.options.iter().find(|option| option.broad).unwrap();
        assert!(!broad.is_default);
        assert_eq!(broad.allowed_lifetimes, [PermissionLifetime::Conversation]);
        assert!(matches!(
            broad.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert!(
            request
                .options
                .iter()
                .filter(|option| option.rule.effect == StructuredPermissionEffect::Allow)
                .filter(|option| !option.broad)
                .all(|option| matches!(
                    option.rule.arguments,
                    PermissionArgumentConstraint::Exact { .. }
                ))
        );
        for lifetime in [
            PermissionLifetime::Once,
            PermissionLifetime::Conversation,
            PermissionLifetime::Project,
            PermissionLifetime::Global,
        ] {
            assert!(request.options.iter().any(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && !option.broad
                    && option.allowed_lifetimes.contains(&lifetime)
            }));
        }
        let serialized = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<PermissionRequest>(&serialized).unwrap(),
            request
        );
    }

    const EXPECT_STRICT_URL: &str = "the value is a strict HTTP(S) URL";
    const EXPECT_URL_ROOTS: &str = "a strict HTTP(S) URL has subtree roots";
    const EXPECT_SUBTREE_OPTION: &str = "a webfetch request offers a URL subtree";

    fn webfetch_request(url: &str) -> PermissionRequest {
        PermissionRequest::from_legacy(
            url.into(),
            ToolKey::native("webfetch"),
            vec![url.into()],
            json!({"url": url}),
            Path::new("/project"),
            false,
        )
    }

    #[test]
    fn url_subtree_roots_climb_from_the_path_to_the_origin() {
        let strict =
            strict_http_url("https://example.com/a/b/c?q=1#frag").expect(EXPECT_STRICT_URL);
        assert_eq!(
            url_subtree_roots(&strict).expect(EXPECT_URL_ROOTS),
            [
                "https://example.com/a/b/c",
                "https://example.com/a/b",
                "https://example.com/a",
                "https://example.com/",
            ]
        );
    }

    /// The root a rule is minted from and the roots a match is tested against
    /// have to be spelled the same way, or a grant fails to cover the very URL
    /// it was granted for. An empty path segment is where the two spellings used
    /// to diverge.
    #[test]
    fn a_repeated_slash_mints_a_rule_that_matches_its_own_url() {
        let request = webfetch_request("https://example.com/a//b");
        let subtree = request
            .option_rule("allow_url_subtree", PermissionLifetime::Conversation)
            .expect(EXPECT_SUBTREE_OPTION);
        assert!(permission_rule_covers_request(&subtree, &request));
    }

    #[test]
    fn webfetch_options_cover_exact_url_subtree_origin_and_any_url() {
        let request = PermissionRequest::from_legacy(
            "webfetch".into(),
            ToolKey::native("webfetch"),
            vec!["http://example.com/docs/page?token=secret#fragment".into()],
            json!({
                "url": "http://example.com/docs/page?token=secret#fragment",
                "format": "markdown"
            }),
            Path::new("/project"),
            false,
        );
        for option in [
            "allow_exact",
            "allow_exact_url",
            "allow_url_subtree",
            "allow_url_origin",
            "allow_any_url",
        ] {
            assert!(
                request
                    .options
                    .iter()
                    .any(|candidate| candidate.id == option),
                "missing {option}"
            );
        }
        assert_eq!(
            request.resources[0].value,
            "https://example.com/docs/page?token=secret"
        );
        assert!(!request.presentation.resources[0].summary.contains("secret"));

        let descendant = PermissionRequest::from_legacy(
            "descendant".into(),
            ToolKey::native("webfetch"),
            vec!["https://example.com/docs/page/child?other=value".into()],
            json!({"url": "https://example.com/docs/page/child?other=value", "timeout": 10}),
            Path::new("/project"),
            false,
        );
        let sibling = PermissionRequest::from_legacy(
            "sibling".into(),
            ToolKey::native("webfetch"),
            vec!["https://example.com/docs/other".into()],
            json!({"url": "https://example.com/docs/other"}),
            Path::new("/project"),
            false,
        );
        let other_origin = PermissionRequest::from_legacy(
            "other".into(),
            ToolKey::native("webfetch"),
            vec!["https://other.example/path".into()],
            json!({"url": "https://other.example/path"}),
            Path::new("/project"),
            false,
        );

        let subtree = request
            .option_rule("allow_url_subtree", PermissionLifetime::Conversation)
            .unwrap();
        assert!(permission_rule_covers_request(&subtree, &descendant));
        assert!(!permission_rule_covers_request(&subtree, &sibling));
        let origin = request
            .option_rule("allow_url_origin", PermissionLifetime::Project)
            .unwrap();
        assert!(permission_rule_covers_request(&origin, &sibling));
        assert!(!permission_rule_covers_request(&origin, &other_origin));
        let any = request
            .option_rule("allow_any_url", PermissionLifetime::Global)
            .unwrap();
        assert!(permission_rule_covers_request(&any, &other_origin));

        let persisted = serde_json::to_string(&any).unwrap();
        assert!(!persisted.contains("example.com"));
        assert!(!persisted.contains("secret"));
    }

    #[test]
    fn shell_pattern_option_preserves_exact_fallbacks_and_resource_context() {
        let resources = vec![
            command_resource("git diff --stat", "/project"),
            command_resource("git status --short", "/project"),
            command_resource(r#"printf "%s\n" done"#, "/project"),
        ];
        let exact_constraints = exact_resource_constraints(&resources);
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            resources,
            json!({"command": "multiple", "timeout": 30}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "These command patterns in this workdir");
        assert_eq!(
            option.description,
            r#"Allow commands matching these patterns in /project: git diff *, git status *. Also allow `printf "%s\n" done` exactly as reviewed."#
        );
        assert!(option.broad);
        assert!(!option.is_default);
        assert_eq!(option.confirmation, None);
        assert_eq!(
            option.allowed_lifetimes,
            [
                PermissionLifetime::Conversation,
                PermissionLifetime::Project,
                PermissionLifetime::Global,
            ]
        );
        assert!(matches!(
            option.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert!(matches!(
            &option.rule.resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git diff *"
        ));
        assert!(matches!(
            &option.rule.resources[1].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git status *"
        ));
        assert_eq!(
            option.rule.resources[2].selector,
            exact_constraints[2].selector
        );
        for (constraint, exact) in option.rule.resources.iter().zip(&exact_constraints) {
            assert_eq!(constraint.kind, exact.kind);
            assert_eq!(constraint.access, exact.access);
            assert_eq!(constraint.protected, exact.protected);
            assert_eq!(constraint.attributes, exact.attributes);
        }
    }

    #[test]
    fn shell_pattern_option_uses_a_single_prefix_label() {
        let mut resource = command_resource("/usr/bin/git diff --stat", "/project");
        resource.attributes.insert(
            super::NORMALIZED_COMMAND_ATTRIBUTE.into(),
            "git diff --stat".into(),
        );
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![resource],
            json!({"command": "/usr/bin/git diff --stat"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(
            option.label,
            "Any `/usr/bin/git diff` command in this workdir"
        );
        assert!(
            !option.rule.resources[0]
                .attributes
                .contains_key(super::NORMALIZED_COMMAND_ATTRIBUTE)
        );

        let mut next_resource = command_resource("/usr/bin/git diff --check", "/project");
        next_resource.attributes.insert(
            super::NORMALIZED_COMMAND_ATTRIBUTE.into(),
            "git diff --check".into(),
        );
        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![next_resource],
            json!({"command": "/usr/bin/git diff --check"}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_reuses_a_reviewed_subcommand() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource(r#"git commit -m "first""#, "/project")],
            json!({"command": r#"git commit -m "first""#}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "Any `git commit` command in this workdir");

        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource(r#"git commit -m "second""#, "/project")],
            json!({"command": r#"git commit -m "second""#}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_keeps_a_curated_search_term_out_of_the_rule() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("rg needle src/", "/project")],
            json!({"command": "rg needle src/"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_command_patterns")
            .unwrap();

        assert_eq!(option.label, "Any `rg` command in this workdir");

        let next = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("rg other tests/", "/project")],
            json!({"command": "rg other tests/"}),
        );
        assert!(permission_rule_covers_request(&option.rule, &next));
    }

    #[test]
    fn shell_pattern_option_is_absent_for_builtin_ask_families() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git checkout main --force", "/project")],
            json!({"command": "git checkout main --force"}),
        );

        assert!(
            request
                .options
                .iter()
                .all(|option| option.id != "allow_command_patterns")
        );
    }

    #[test]
    fn shell_pattern_option_is_absent_without_a_reusable_prefix() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("rm -rf /tmp/build", "/project"),
                command_resource(r#"printf "%s\n" done"#, "/project"),
            ],
            json!({"command": "multiple"}),
        );

        assert!(
            request
                .options
                .iter()
                .all(|option| option.id != "allow_command_patterns")
        );
    }

    #[test]
    fn exact_command_option_names_every_reviewed_command_and_workdir() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("cargo test", "/project"),
                command_resource("git status --short", "/project"),
            ],
            json!({"command": "multiple"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_exact_commands")
            .unwrap();

        assert_eq!(option.label, "These commands in this workdir");
        assert_eq!(
            option.description,
            "Allow `cargo test`, `git status --short` in /project with different timeout or display controls."
        );
    }

    #[test]
    fn exact_command_option_uses_a_singular_label_for_one_command() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("cargo test", "/project")],
            json!({"command": "cargo test"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_exact_commands")
            .unwrap();

        assert_eq!(option.label, "This command in this workdir");
        assert_eq!(
            option.description,
            "Allow `cargo test` in /project with different timeout or display controls."
        );
    }

    #[test]
    fn each_command_earns_a_ladder_that_speaks_only_for_itself() {
        let resources = vec![
            command_resource("git status --short", "/project"),
            command_resource(r#"printf "%s\n" done"#, "/project"),
        ];
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            resources.clone(),
            json!({"command": "multiple"}),
        );
        let rungs = |index: usize| {
            request
                .options
                .iter()
                .filter(|option| {
                    option.group.as_ref().and_then(|group| group.resource) == Some(index)
                })
                .collect::<Vec<_>>()
        };
        let chips = |index: usize| {
            rungs(index)
                .into_iter()
                .map(|option| {
                    (
                        option.id.clone(),
                        option.group.as_ref().unwrap().value.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            chips(0),
            [
                ("command_exact_0".to_owned(), EXACT_COMMAND_CHIP.to_owned()),
                ("command_pattern_0".to_owned(), "git status *".to_owned()),
            ]
        );
        assert_eq!(
            chips(1),
            [("command_exact_1".to_owned(), EXACT_COMMAND_CHIP.to_owned())]
        );
        for index in 0..resources.len() {
            for option in rungs(index) {
                assert_eq!(
                    option.group.as_ref().unwrap().key,
                    format!("command_{index}")
                );
                assert_eq!(option.rule.resources.len(), 1);
                assert!(permission_rule_covers_resource(
                    &option.rule,
                    &request,
                    &resources[index]
                ));
                assert!(!permission_rule_covers_resource(
                    &option.rule,
                    &request,
                    &resources[1 - index]
                ));
            }
        }
    }

    fn two_command_request() -> PermissionRequest {
        explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("git status --short", "/project"),
                command_resource("cargo test", "/project"),
            ],
            json!({"command": "multiple"}),
        )
    }

    fn composed(
        request: &PermissionRequest,
        rows: Vec<Option<PermissionRowGrant>>,
    ) -> Result<Vec<StructuredPermissionRule>, ComposedAnswerError> {
        request.composed_rules(&rows, &PermissionLifetime::Conversation)
    }

    const SHARED_PATTERN_COMMANDS: [&str; 2] = ["git status --short", "git status --porcelain"];
    const BUILTIN_ALLOW_AUTHORITY: &str = "echo *";

    /// Two commands one pattern reaches, which is how a pipeline that greps
    /// twice arrives at the prompt.
    fn twin_command_request() -> PermissionRequest {
        explicit_request(
            PermissionAuthorityProfile::Shell,
            SHARED_PATTERN_COMMANDS
                .iter()
                .map(|command| command_resource(command, "/project"))
                .collect(),
            json!({"command": "twice"}),
        )
    }

    fn offered(id: &str) -> Option<PermissionRowGrant> {
        Some(PermissionRowGrant::Offered(id.into()))
    }

    /// Two rows reaching the same pattern reach each other, so exactly one has
    /// to survive or the answer files the same rule twice.
    #[test]
    fn rows_that_reach_each_other_leave_only_the_first_standing() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[offered("command_pattern_0"), offered("command_pattern_1")]),
            vec![None, Some(0)]
        );
    }

    /// A row pinned to its own command contributes nothing under a row that
    /// reaches it, whichever way round they sit.
    #[test]
    fn a_narrower_row_is_subsumed_from_either_direction() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[offered("command_exact_0"), offered("command_pattern_1")]),
            vec![Some(1), None]
        );
        assert_eq!(
            request.subsumed_rows(&[offered("command_pattern_0"), offered("command_exact_1")]),
            vec![None, Some(0)]
        );
    }

    /// Narrowing the covering row hands the subsumed row its own choice back,
    /// so nothing has to be remembered across the change.
    #[test]
    fn narrowing_the_covering_row_leaves_nothing_subsumed() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[offered("command_exact_0"), offered("command_exact_1")]),
            vec![None, None]
        );
    }

    /// A row granting nothing neither subsumes nor is subsumed: it is not part
    /// of the answer at all.
    #[test]
    fn a_row_that_grants_nothing_stays_out_of_subsumption() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[None, offered("command_pattern_1")]),
            vec![None, None]
        );
    }

    fn covered_at(
        request: &mut PermissionRequest,
        index: usize,
        origin: RuleOrigin,
        authority: &str,
    ) {
        request.presentation.resources[index].coverage = Some(ResourceCoverage {
            origin,
            authority: authority.into(),
        });
    }

    /// Two rows reaching the same pattern once produced one rule holding the
    /// same constraint twice; as separate rules they would be twins that a
    /// single revoke could not remove together.
    #[test]
    fn rows_that_reach_each_other_file_one_rule() {
        let request = twin_command_request();

        let rules = composed(
            &request,
            vec![offered("command_pattern_0"), offered("command_pattern_1")],
        )
        .unwrap();

        assert_eq!(rules.len(), 1);
        assert!(
            request
                .resources
                .iter()
                .all(|resource| permission_rule_covers_resource(&rules[0], &request, resource))
        );
    }

    /// Each surviving row is a rule of its own, so one command can be revoked
    /// without touching the others.
    #[test]
    fn every_surviving_row_files_a_rule_of_its_own() {
        let request = two_command_request();

        let rules = composed(
            &request,
            vec![offered("command_exact_0"), offered("command_exact_1")],
        )
        .unwrap();

        assert_eq!(rules.len(), 2);
        for (index, rule) in rules.iter().enumerate() {
            assert_eq!(rule.resources.len(), 1);
            assert!(permission_rule_covers_resource(
                rule,
                &request,
                &request.resources[index]
            ));
            assert!(!permission_rule_covers_resource(
                rule,
                &request,
                &request.resources[1 - index]
            ));
        }
    }

    /// Re-granting what is already covered as durably files nothing, which is
    /// what keeps the inventory free of rules that change no decision.
    #[test]
    fn a_row_already_covered_as_durably_files_nothing() {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Project, NARROW_ALLOW);
        let rows = vec![offered("command_exact_0"), None];

        assert_eq!(
            request.composed_rules(&rows, &PermissionLifetime::Project),
            Ok(Vec::new())
        );
        assert_eq!(
            request
                .composed_rules(&rows, &PermissionLifetime::Conversation)
                .map(|rules| rules.len()),
            Ok(0)
        );
    }

    /// Coverage that expires sooner than the grant is not enough: pruning there
    /// would drop the grant and let the authority disappear with the session.
    #[test]
    fn a_row_covered_less_durably_than_the_grant_still_files() {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Conversation, NARROW_ALLOW);
        let rows = vec![offered("command_exact_0"), None];

        assert_eq!(
            request
                .composed_rules(&rows, &PermissionLifetime::Project)
                .map(|rules| rules.len()),
            Ok(1)
        );
    }

    /// The builtin allowlist is a default consulted only where no rule speaks,
    /// so a grant over it is a real grant however durable it looks.
    #[test]
    fn a_row_covered_only_by_the_builtin_allowlist_still_files() {
        let mut request = two_command_request();
        covered_at(
            &mut request,
            0,
            RuleOrigin::Builtin,
            BUILTIN_ALLOW_AUTHORITY,
        );
        let rows = vec![offered("command_exact_0"), None];

        assert_eq!(
            request
                .composed_rules(&rows, &PermissionLifetime::Global)
                .map(|rules| rules.len()),
            Ok(1)
        );
    }

    /// Widening a covered row reaches commands the coverage may not, so it is
    /// a real grant even though this command was already allowed.
    #[test]
    fn widening_a_covered_row_still_files() {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Project, EXACT_COMMAND_CHIP);
        let rows = vec![offered("command_pattern_0"), None];

        assert_eq!(
            request
                .composed_rules(&rows, &PermissionLifetime::Project)
                .map(|rules| rules.len()),
            Ok(1)
        );

        covered_at(&mut request, 0, RuleOrigin::Project, NARROW_ALLOW);
        assert_eq!(
            request.composed_rules(&rows, &PermissionLifetime::Project),
            Ok(Vec::new()),
            "naming the stored pattern reaches no further than it does"
        );
    }

    /// A pattern typed for one row still reaches the other, so it subsumes the
    /// same way an offered rung does.
    #[test]
    fn a_written_pattern_subsumes_the_row_it_reaches() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[
                Some(PermissionRowGrant::Written(BROAD_ASK.into())),
                offered("command_exact_1"),
            ]),
            vec![None, Some(0)]
        );
    }

    #[test]
    fn a_composed_answer_keeps_each_row_at_the_breadth_it_chose() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                Some(PermissionRowGrant::Offered("command_pattern_0".into())),
                Some(PermissionRowGrant::Offered("command_exact_1".into())),
            ],
        )
        .unwrap();

        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .all(|rule| matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained))
        );
        assert!(matches!(
            &rules[0].resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git status *"
        ));
        assert_eq!(
            rules[1].resources[0].selector,
            resource_constraint(&request.resources[1]).selector
        );
        // The widened row reaches beyond what was reviewed; the pinned one does not.
        assert!(permission_rule_covers_resource(
            &rules[0],
            &request,
            &command_resource("git status --porcelain", "/project")
        ));
        assert!(!permission_rule_covers_resource(
            &rules[1],
            &request,
            &command_resource("cargo test --lib", "/project")
        ));
    }

    /// A row left ungranted must contribute no constraint. An empty constraint
    /// list is an unrestricted rule, so the answer has to store nothing at all
    /// rather than store a rule that happens to name nothing.
    #[test]
    fn a_row_left_ungranted_is_absent_from_the_stored_rule() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                Some(PermissionRowGrant::Offered("command_exact_0".into())),
                None,
            ],
        )
        .unwrap();

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].resources.len(), 1);
        assert!(permission_rule_covers_resource(
            &rules[0],
            &request,
            &request.resources[0]
        ));
        assert!(!permission_rule_covers_resource(
            &rules[0],
            &request,
            &request.resources[1]
        ));
        assert_eq!(composed(&request, vec![None, None]), Ok(Vec::new()));
    }

    #[test]
    fn a_written_pattern_is_admitted_only_against_its_own_command() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                None,
                Some(PermissionRowGrant::Written("cargo test *".into())),
            ],
        )
        .unwrap();

        assert!(matches!(
            &rules[0].resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "cargo test *"
        ));
        assert_eq!(
            composed(
                &request,
                vec![
                    Some(PermissionRowGrant::Written("cargo test *".into())),
                    None
                ],
            ),
            Err(ComposedAnswerError::Pattern {
                command: "git status --short".into(),
                fault: PatternFault::DoesNotMatch,
            })
        );
    }

    #[test]
    fn a_row_may_not_borrow_another_rows_rung() {
        let request = two_command_request();

        assert_eq!(
            composed(
                &request,
                vec![
                    Some(PermissionRowGrant::Offered("command_exact_1".into())),
                    None,
                ],
            ),
            Err(ComposedAnswerError::NotOffered("command_exact_1".into()))
        );
        assert_eq!(
            composed(&request, vec![None]),
            Err(ComposedAnswerError::RowCount {
                named: 1,
                resources: 2,
            })
        );
    }

    /// Withdrawing a row's lifetimes is how plan mode contains authority, and a
    /// written pattern must not be a way around it.
    #[test]
    fn a_written_pattern_inherits_the_lifetimes_its_row_still_allows() {
        let mut request = two_command_request();
        for option in &mut request.options {
            option.allowed_lifetimes.retain(|lifetime| {
                !matches!(
                    lifetime,
                    PermissionLifetime::Project | PermissionLifetime::Global
                )
            });
        }
        let rows = vec![
            None,
            Some(PermissionRowGrant::Written("cargo test *".into())),
        ];

        assert!(
            request
                .composed_rules(&rows, &PermissionLifetime::Conversation)
                .is_ok()
        );
        assert_eq!(
            request.composed_rules(&rows, &PermissionLifetime::Project),
            Err(ComposedAnswerError::LifetimeWithdrawn(
                "command_exact_1".into()
            ))
        );
    }

    #[test]
    fn blanket_workdir_authority_reaches_every_reviewed_workdir() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("git status", "/project"),
                command_resource("cargo test", "/other"),
            ],
            json!({"command": "multiple"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_commands_in_workdir")
            .expect("workdir authority option");

        assert_eq!(option.label, "Any command in these workdirs");
        assert_eq!(
            option.description,
            "Allow arbitrary commands starting in `/other`, `/project`."
        );
        assert!(permission_rule_covers_request(&option.rule, &request));
    }

    #[test]
    fn listed_commands_cap_the_enumeration_and_escape_control_characters() {
        let commands = ["one", "two", "three", "four\nfive"];

        assert_eq!(
            listed_commands(commands.iter().copied()),
            "`one`, `two`, `three`, +1 more"
        );
        assert_eq!(
            listed_commands(commands[3..].iter().copied()),
            r"`four\nfive`"
        );
    }

    #[test]
    fn protected_commands_offer_only_exact_and_blanket_authorities() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/status",
                "/project",
            )],
            json!({"command": "git status > /tmp/status"}),
        );

        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            [
                "allow_exact",
                "deny_exact",
                "allow_commands_in_workdir",
                "allow_any_command",
            ]
        );
    }

    #[test_case("allow_commands_in_workdir", true; "workdir_authority")]
    #[test_case("allow_any_command", true; "global_authority")]
    #[test_case("allow_command_patterns", false; "pattern_authority")]
    #[test_case("allow_exact_commands", false; "exact_command_authority")]
    fn broad_shell_authority_reaches_protected_commands(option_id: &str, covers: bool) {
        let reviewed = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git status --short", "/project")],
            json!({"command": "git status --short"}),
        );
        let rule = reviewed
            .options
            .iter()
            .find(|option| option.id == option_id)
            .expect("shell authority option")
            .rule
            .clone();
        let protected = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/out",
                "/project",
            )],
            json!({"command": "git status > /tmp/out"}),
        );

        assert_eq!(permission_rule_covers_request(&rule, &protected), covers);
    }

    #[test]
    fn broad_shell_authority_stops_at_its_workdir() {
        let reviewed = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git status", "/project")],
            json!({"command": "git status"}),
        );
        let rule = reviewed
            .options
            .iter()
            .find(|option| option.id == "allow_commands_in_workdir")
            .expect("workdir authority option")
            .rule
            .clone();
        let elsewhere = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/out",
                "/other",
            )],
            json!({"command": "git status > /tmp/out"}),
        );

        assert!(!permission_rule_covers_request(&rule, &elsewhere));
    }

    #[test_case(PermissionResourceKind::Command, true; "protected_command")]
    #[test_case(PermissionResourceKind::File, false; "protected_file")]
    #[test_case(PermissionResourceKind::Directory, false; "protected_directory")]
    fn blanket_selectors_reach_protected_commands_only(
        kind: PermissionResourceKind,
        matches: bool,
    ) {
        let constraint = PermissionResourceConstraint {
            kind: kind.clone(),
            selector: PermissionResourceSelector::Any,
            access: Some(PermissionResourceAccess::Execute),
            protected: None,
            attributes: BTreeMap::new(),
        };
        let resource = PermissionResource {
            kind,
            value: "/etc/shadow".into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: true,
            requires_prompt: true,
            attributes: BTreeMap::new(),
        };

        assert_eq!(resource_constraint_matches(&constraint, &resource), matches);
    }

    const EXACT_RESOURCES_OPTION: &str = "allow_exact_resources";
    const SUBTREE_OPTION: &str = "allow_filesystem_subtree";
    /// The first rung above the resource's own directory. For a path one level
    /// inside the project that rung is the project root itself.
    const PROJECT_RUNG: &str = "allow_filesystem_subtree_1";
    const PROJECT_ROOT_MARK: &str = "(project root)";

    const PROTECTED_PATH: &str = "/project/.env";
    const FIRST_READ_OFFSET: u32 = 1;
    const LATER_READ_OFFSET: u32 = 500;

    fn filesystem_request(protected: bool, offset: u32) -> PermissionRequest {
        explicit_request(
            PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            },
            vec![PermissionResource {
                kind: PermissionResourceKind::File,
                value: PROTECTED_PATH.into(),
                access: Some(PermissionResourceAccess::Read),
                protected,
                requires_prompt: protected,
                attributes: BTreeMap::new(),
            }],
            json!({"path": PROTECTED_PATH, "offset": offset}),
        )
    }

    fn option_ids(request: &PermissionRequest) -> Vec<&str> {
        request
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect()
    }

    #[test]
    fn a_protected_path_is_offered_a_reusable_exact_path_grant() {
        let request = filesystem_request(true, FIRST_READ_OFFSET);

        let option = request
            .options
            .iter()
            .find(|option| option.id == EXACT_RESOURCES_OPTION)
            .expect("protected path must still earn an exact-path option");

        assert!(matches!(
            option.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert_eq!(option.rule.resources[0].protected, Some(true));
        assert!(
            option
                .allowed_lifetimes
                .contains(&PermissionLifetime::Conversation)
        );
    }

    #[test]
    fn a_protected_path_is_never_offered_a_subtree_grant() {
        let request = filesystem_request(true, FIRST_READ_OFFSET);
        let ids = option_ids(&request);

        assert!(
            !ids.iter().any(|id| id.starts_with(SUBTREE_OPTION)),
            "{ids:?}"
        );
    }

    #[test]
    fn an_unprotected_path_still_earns_every_filesystem_grant() {
        let request = filesystem_request(false, FIRST_READ_OFFSET);
        let ids = option_ids(&request);

        assert!(ids.contains(&EXACT_RESOURCES_OPTION), "{ids:?}");
        assert!(ids.contains(&SUBTREE_OPTION), "{ids:?}");
        assert!(
            request.options.iter().any(|option| option
                .group
                .as_ref()
                .is_some_and(|group| group.value.contains(PROJECT_ROOT_MARK))),
            "{ids:?}"
        );
    }

    #[test]
    fn a_reusable_protected_grant_covers_the_same_path_under_a_different_input() {
        let granted = filesystem_request(true, FIRST_READ_OFFSET);
        let rule = granted
            .option_rule(EXACT_RESOURCES_OPTION, PermissionLifetime::Conversation)
            .expect("protected path must still earn an exact-path option");

        let repeat = filesystem_request(true, LATER_READ_OFFSET);

        assert_ne!(granted.input_digest, repeat.input_digest);
        assert!(permission_rule_covers_request(&rule, &repeat));
    }

    const PROJECT_ROOT: &str = "/project";

    fn flags_in_project(path: &str, access: PermissionResourceAccess) -> (bool, bool) {
        filesystem_resource_flags(path, &access, Path::new(PROJECT_ROOT))
    }

    /// Inert git bookkeeping is the repository describing itself: reading it
    /// leaks nothing and mutates nothing, so it must not force a prompt. The
    /// guard has to survive for `config` and `hooks`, which carry remote
    /// credentials and executable content, and for every write.
    #[test_case("/project/.git/HEAD", PermissionResourceAccess::Read => (false, false) ; "head_read")]
    #[test_case("/project/.git/refs/heads/main", PermissionResourceAccess::Read => (false, false) ; "refs_read")]
    #[test_case("/project/.git/logs/HEAD", PermissionResourceAccess::Read => (false, false) ; "reflog_read")]
    #[test_case("/project/.git/objects/ab/cdef", PermissionResourceAccess::Search => (false, false) ; "objects_search")]
    #[test_case("/project/.git/config", PermissionResourceAccess::Read => (true, true) ; "config_stays_guarded")]
    #[test_case("/project/.git/hooks/pre-commit", PermissionResourceAccess::Read => (true, true) ; "hooks_stay_guarded")]
    #[test_case("/project/.git/refs/../config", PermissionResourceAccess::Read => (true, true) ; "traversal_into_config_stays_guarded")]
    #[test_case("/project/.git", PermissionResourceAccess::Read => (true, true) ; "the_directory_itself_stays_guarded")]
    #[test_case("/project/vendor/dep/.git/HEAD", PermissionResourceAccess::Read => (true, true) ; "a_nested_checkout_is_not_the_projects_own_git")]
    #[test_case("/project/.git/HEAD", PermissionResourceAccess::Write => (true, true) ; "writes_stay_guarded")]
    #[test_case("/elsewhere/.git/HEAD", PermissionResourceAccess::Read => (true, true) ; "outside_the_project_stays_guarded")]
    #[test_case("/project/.ssh/id_rsa", PermissionResourceAccess::Read => (true, true) ; "ssh_is_untouched")]
    #[test_case("/project/.env.local", PermissionResourceAccess::Read => (true, true) ; "dotenv_is_untouched")]
    #[test_case("/project/src/main.rs", PermissionResourceAccess::Read => (false, false) ; "ordinary_project_file")]
    #[test_case("/elsewhere/notes.md", PermissionResourceAccess::Read => (false, true) ; "ordinary_file_outside_the_project")]
    fn filesystem_flags_exempt_only_inert_git_reads(
        path: &str,
        access: PermissionResourceAccess,
    ) -> (bool, bool) {
        flags_in_project(path, access)
    }

    /// The exemption is anchored to the working directory, not to the enclosing
    /// repository, so running from a subdirectory must not silently widen it.
    #[test]
    fn the_git_exemption_does_not_reach_above_the_working_directory() {
        assert_eq!(
            filesystem_resource_flags(
                "/project/.git/HEAD",
                &PermissionResourceAccess::Read,
                Path::new("/project/src"),
            ),
            (true, true)
        );
    }

    const READ_CONTRACT: &str = "file.read.v1";
    const GREP_CONTRACT: &str = "file.grep.v1";
    const WRITE_CONTRACT: &str = "file.write.v1";
    const SOURCE_FILE: &str = "/project/src/main.rs";
    const SOURCE_DIR: &str = "/project/src";

    fn workcell_request(
        contract: &str,
        kind: PermissionResourceKind,
        access: PermissionResourceAccess,
        value: &str,
    ) -> PermissionRequest {
        let intent = PermissionIntent::new(
            PermissionScopes::single(value.to_owned()),
            vec![PermissionResource {
                kind,
                value: value.into(),
                access: Some(access),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "path": value }),
            Path::new("/project"),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: contract.into(),
            },
            PermissionExecutorKind::Native,
        )
    }

    fn read_subtree_rule(option: &str) -> StructuredPermissionRule {
        workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        )
        .option_rule(option, PermissionLifetime::Conversation)
        .expect("a first-party read must offer a subtree grant")
    }

    /// Every rung of the subtree ladder, in the order it was offered.
    fn subtree_ladder(request: &PermissionRequest) -> Vec<&PermissionRuleOption> {
        request
            .options
            .iter()
            .filter(|option| {
                option
                    .group
                    .as_ref()
                    .is_some_and(|group| group.key == SUBTREE_OPTION)
            })
            .collect()
    }

    fn ladder_values(request: &PermissionRequest) -> Vec<String> {
        subtree_ladder(request)
            .iter()
            .map(|option| {
                option
                    .group
                    .as_ref()
                    .expect("a rung is grouped")
                    .value
                    .clone()
            })
            .collect()
    }

    /// A file two levels inside the project can be widened all the way out, so
    /// reading a sibling checkout no longer stops at the file's own directory.
    #[test]
    fn the_subtree_ladder_climbs_to_the_filesystem_root() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );

        assert_eq!(
            ladder_values(&request),
            vec![
                "/project/src/**".to_string(),
                format!("/project/** {PROJECT_ROOT_MARK}"),
                "/**".to_string(),
            ]
        );
        assert_eq!(
            subtree_ladder(&request)
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec![SUBTREE_OPTION, PROJECT_RUNG, "allow_filesystem_subtree_2"]
        );
    }

    /// The widest rung still has to grant what the request asked for, or the
    /// ladder is a row of labels that authorise nothing.
    #[test]
    fn the_widest_rung_still_covers_the_requested_path() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let widest = subtree_ladder(&request)
            .last()
            .expect("the ladder has a widest rung")
            .id
            .clone();
        let rule = request
            .option_rule(&widest, PermissionLifetime::Conversation)
            .expect("the widest rung is grantable");

        assert!(permission_rule_covers_request(&rule, &request));
    }

    /// The ladder may reach the filesystem root, so this is what keeps a
    /// credential out of reach however far it is widened. Two guards say so
    /// independently — every rung pins `protected: Some(false)`, and a subtree
    /// selector is not exact enough to grant a protected resource — so the
    /// outcome is asserted rather than either mechanism.
    #[test]
    fn no_rung_can_reach_a_protected_path() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let intent = PermissionIntent::new(
            PermissionScopes::single(PROTECTED_PATH.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::File,
                value: PROTECTED_PATH.into(),
                access: Some(PermissionResourceAccess::Read),
                protected: true,
                requires_prompt: true,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let secret = PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "path": PROTECTED_PATH }),
            Path::new("/project"),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );

        for rung in subtree_ladder(&request) {
            let rule = request
                .option_rule(&rung.id, PermissionLifetime::Conversation)
                .expect("a rung is grantable");
            assert!(
                !permission_rule_covers_request(&rule, &secret),
                "{} reached {PROTECTED_PATH}",
                rung.id
            );
        }
    }

    /// Two resources in sibling directories share no rung until their common
    /// ancestor, so that is where the ladder starts climbing.
    #[test]
    fn a_split_request_starts_climbing_at_the_common_ancestor() {
        let intent = PermissionIntent::new(
            PermissionScopes::single("split".into()),
            ["/project/src/main.rs", "/project/tests/it.rs"]
                .into_iter()
                .map(|value| PermissionResource {
                    kind: PermissionResourceKind::File,
                    value: value.into(),
                    access: Some(PermissionResourceAccess::Read),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                })
                .collect(),
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let request = PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "paths": ["/project/src/main.rs", "/project/tests/it.rs"] }),
            Path::new("/project"),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );

        assert_eq!(
            ladder_values(&request),
            vec![
                "/project/src/**, /project/tests/**".to_string(),
                format!("/project/** {PROJECT_ROOT_MARK}"),
                "/**".to_string(),
            ]
        );
    }

    fn caution_of(root: &Path, resource: &Path) -> Vec<Option<PermissionCaution>> {
        let value = resource.to_string_lossy().into_owned();
        let intent = PermissionIntent::new(
            PermissionScopes::single(value.clone()),
            vec![PermissionResource {
                kind: PermissionResourceKind::File,
                value: value.clone(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let request = PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "path": value }),
            root,
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );
        subtree_ladder(&request)
            .iter()
            .map(|option| option.caution)
            .collect()
    }

    /// Widening is uncautioned inside the repository, warned once it reaches
    /// past it, and grave once the grant would swallow the home directory.
    #[test]
    fn the_ladder_cautions_each_rung_by_what_it_reaches() {
        let Some(home) = caudra_storage::paths::home() else {
            return;
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let repository = temp.path().join("repo");
        std::fs::create_dir_all(repository.join(GIT_METADATA_DIR)).expect("git marker");
        std::fs::create_dir_all(repository.join("src")).expect("source dir");

        // A repository outside home: its own rungs are clean, the rungs above
        // it warn, and only the root — which contains home — is grave. Sitting
        // outside home is not the same as reaching over it.
        let cautions = caution_of(&repository, &repository.join("src/main.rs"));
        assert_eq!(cautions.first(), Some(&None));
        assert!(
            cautions.contains(&Some(PermissionCaution::Warn)),
            "{cautions:?}"
        );
        assert_eq!(cautions.last(), Some(&Some(PermissionCaution::Danger)));
        assert_eq!(
            cautions
                .iter()
                .filter(|caution| **caution == Some(PermissionCaution::Danger))
                .count(),
            1,
            "{cautions:?}"
        );

        // A path under home with no repository at all warns from the start and
        // still ends grave at the root.
        let cautions = caution_of(&home, &home.join("notes/todo.md"));
        assert_eq!(cautions.first(), Some(&Some(PermissionCaution::Warn)));
        assert_eq!(cautions.last(), Some(&Some(PermissionCaution::Danger)));
    }

    /// A grant that swallows home has to be typed out, whatever else it does.
    #[test]
    fn a_rung_outside_home_demands_the_typed_phrase() {
        let request = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        let ladder = subtree_ladder(&request);
        let widest = ladder.last().expect("the ladder has a widest rung");

        assert_eq!(widest.caution, Some(PermissionCaution::Danger));
        assert_eq!(widest.confirmation.as_deref(), Some(OUTSIDE_HOME_PHRASE));
    }

    #[test]
    fn a_read_subtree_grant_covers_a_later_search_by_another_contract() {
        let rule = read_subtree_rule(SUBTREE_OPTION);

        let grep = workcell_request(
            GREP_CONTRACT,
            PermissionResourceKind::Directory,
            PermissionResourceAccess::Search,
            SOURCE_DIR,
        );

        assert_ne!(rule.subject, grep.subject);
        assert!(permission_rule_covers_request(&rule, &grep));
    }

    #[test]
    fn a_read_subtree_grant_never_covers_a_write_to_the_same_subtree() {
        let rule = read_subtree_rule(SUBTREE_OPTION);

        let write = workcell_request(
            WRITE_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Write,
            SOURCE_FILE,
        );

        assert!(!permission_rule_covers_request(&rule, &write));
    }

    /// The subject check already rejects a real write, since the write contracts
    /// are not family members. This pins the independent resource check, so a
    /// caller presenting a reading contract with a write resource is still
    /// refused rather than relying on subject filtering alone.
    #[test]
    fn a_read_family_grant_refuses_a_write_resource_from_a_reading_contract() {
        let rule = read_subtree_rule(SUBTREE_OPTION);
        let mut forged = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        forged.resources[0].access = Some(PermissionResourceAccess::Write);

        assert_eq!(rule.subject, forged.subject);
        assert!(!permission_rule_covers_request(&rule, &forged));
    }

    #[test]
    fn a_read_family_grant_stays_inside_its_trust_domain() {
        let rule = read_subtree_rule(SUBTREE_OPTION);
        let mut outsider = workcell_request(
            READ_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Read,
            SOURCE_FILE,
        );
        outsider.subject = PermissionSubject::Native {
            owner: NATIVE_OWNER.into(),
            contract: READ_CONTRACT.into(),
        };

        assert!(!permission_rule_covers_request(&rule, &outsider));
    }

    #[test_case(SUBTREE_OPTION; "subtree grant is widened")]
    #[test_case(PROJECT_RUNG; "project grant is widened")]
    fn a_first_party_read_mints_the_filesystem_read_family(option: &str) {
        assert_eq!(
            read_subtree_rule(option).family,
            Some(PermissionCapabilityFamily::FilesystemRead)
        );
    }

    #[test]
    fn the_exact_path_grant_is_never_widened() {
        assert_eq!(read_subtree_rule(EXACT_RESOURCES_OPTION).family, None);
    }

    #[test]
    fn a_write_request_mints_no_family() {
        let rule = workcell_request(
            WRITE_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Write,
            SOURCE_FILE,
        )
        .option_rule(SUBTREE_OPTION, PermissionLifetime::Conversation)
        .expect("a write still offers a subtree grant");

        assert_eq!(rule.family, None);
    }

    #[test]
    fn presentation_coverage_defaults_absent_and_updates_atomically() {
        let resource = PermissionResourcePresentation {
            kind: PermissionResourceKind::Command,
            access: Some(PermissionResourceAccess::Execute),
            summary: "git status".into(),
            protected: false,
            coverage: None,
        };
        let serialized = serde_json::to_value(&resource).unwrap();
        assert!(serialized.get("coverage").is_none());
        let restored: PermissionResourcePresentation = serde_json::from_value(serialized).unwrap();
        assert!(!restored.covered());

        let mut presentation = PermissionPresentation {
            action: "Run commands".into(),
            risk: PermissionRisk::High,
            risk_summary: "Shell execution".into(),
            resources: vec![resource.clone(), resource],
        };
        let granted = ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: NARROW_ALLOW.into(),
        };
        assert!(update_presentation_coverage(
            &mut presentation,
            &[Some(granted), None]
        ));
        assert!(presentation.resources[0].covered());
        assert!(!presentation.resources[1].covered());

        let unchanged = presentation.clone();
        assert!(!update_presentation_coverage(&mut presentation, &[None]));
        assert_eq!(presentation, unchanged);
        assert_eq!(
            serde_json::to_value(&presentation.resources[0]).unwrap()["coverage"],
            json!({"origin": "project", "authority": NARROW_ALLOW})
        );
    }

    #[test]
    fn selected_digest_tracks_presence_without_storing_values() {
        let pointers = vec!["/query".to_owned(), "/country".to_owned()];
        let input = json!({"query": "secret", "limit": 10});
        let constraint = PermissionArgumentConstraint::SelectedDigest {
            digest: selected_input_digest(&input, &pointers).unwrap(),
            pointers,
        };
        assert!(argument_constraint_matches(
            &constraint,
            &json!({"query": "secret", "limit": 20})
        ));
        assert!(!argument_constraint_matches(
            &constraint,
            &json!({"query": "changed", "limit": 10})
        ));
        assert!(!argument_constraint_matches(
            &constraint,
            &json!({"query": "secret", "country": null})
        ));
        assert!(
            !serde_json::to_string(&constraint)
                .unwrap()
                .contains("secret")
        );
    }
}
