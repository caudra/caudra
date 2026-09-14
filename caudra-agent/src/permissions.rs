use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, Weak};
use std::time::Instant;

use caudra_config::{
    DefaultEffect, Effect, FILE_WRITE_TOOLS, PermissionReviewCandidate, PermissionRule,
    PermissionsConfig, ToolKey,
};
use caudra_storage::permission_config_trust::{
    is_project_trusted as is_permission_config_trusted, is_remote_asset_trusted,
    revoke_project_trust as revoke_project_permission_config, revoke_remote_asset_trust,
    trust_project as trust_project_permission_config, trust_remote_asset,
};
use caudra_storage::permission_state::{PermissionState, validate_conversation_record};
use caudra_storage::sessions::SESSIONS_DB_FILE;
use caudra_storage::{StateDir, now_epoch};
use caudra_workspace::ProjectAssetTrustKey;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{info, warn};

use crate::{AgentEvent, EventSender};

mod command_arity;
#[allow(dead_code)]
pub(crate) mod command_pattern;
mod sed_script;
mod structured;
pub use command_pattern::{PatternFault, PatternGrade, grade_command_pattern};
pub use sed_script::sed_only_prints;
pub use structured::*;

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";
/// Set by the shell tool on a command that only observes and can only reach
/// inside the project. Nothing else may set it: the builtin rule below reads it
/// as the whole justification for running without asking.
pub const CONFINED_READ_ATTRIBUTE: &str = "confined_read";
pub const CONFINED_READ_VALUE: &str = "true";
const SHELL_EXECUTION_CONTRACT: &str = "shell.execution.v1";
const WORKCELL_TOOL_OWNER: &str = "workcell";

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

/// Values for the `source` attribute on `caudra.tool_decision` events.
pub const DECISION_SOURCE_RULE: &str = "rule";
pub const DECISION_SOURCE_YOLO: &str = "yolo";
pub const DECISION_SOURCE_USER_ONCE: &str = "user_once";
pub const DECISION_SOURCE_USER_SESSION: &str = "user_session";
pub const DECISION_SOURCE_USER_ALWAYS: &str = "user_always";
pub const DECISION_SOURCE_USER_ABORT: &str = "user_abort";
const SUBTREE_SCOPE_SUFFIX: &str = "/**";
const BASH_WORKDIR_SCOPE_MARKER: &str = " # caudra-workdir[";
const BASH_WORKDIR_FRAME_MARKER: &str = " # caudra-frame[";

/// Wide events for prompt analysis. They land in the ordinary JSON log, which
/// already carries scopes and command text, rather than in telemetry, so the
/// answer to "what keeps prompting me" does not depend on an exporter.
const PERMISSION_LOG_TARGET: &str = "caudra::permission";
const PROMPT_LOG_MAX_RESOURCES: usize = 8;
const PROMPT_LOG_MAX_VALUE_CHARS: usize = 200;

/// Why the request could not be settled from stored authority. Ordered by
/// precedence: the first that applies is reported.
const PROMPT_REASON_FORCED: &str = "force_prompt";
const PROMPT_REASON_PROTECTED: &str = "protected";
const PROMPT_REASON_REQUIRES_PROMPT: &str = "requires_prompt";
const PROMPT_REASON_ASK_RULE: &str = "ask_rule";
const PROMPT_REASON_UNCOVERED: &str = "uncovered";
static NEXT_PERMISSION_MANAGER_ID: AtomicU64 = AtomicU64::new(1);
const PROJECT_READ_TOOLS: &[&str] = &[
    "code_context",
    "code_expand",
    "code_impact",
    "code_map",
    "code_refs",
    "file_glob",
    "file_grep",
    "file_read",
    "glob",
    "grep",
    "file_index",
    "list",
    "read",
    "view_image",
];
/// Tools whose reach is fixed by their own construction rather than by an
/// argument, so a scope would describe nothing a caller can steer.
const TRUSTED_UNSCOPED_TOOLS: &[&str] = &[
    "batch",
    "python_execution",
    "question",
    "skill",
    "task",
    "todo_write",
    "tool_output",
    "workflow",
];

fn builtin_rules(cwd: &Path) -> Vec<PermissionRule> {
    let cwd_glob = format!(
        "{}/**",
        caudra_storage::paths::canonicalize_clean(cwd).display()
    );
    let allow = |tool: &str, scope: &str| PermissionRule {
        tool: ToolKey::native(tool),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    };
    let mut rules: Vec<PermissionRule> = FILE_WRITE_TOOLS
        .iter()
        .map(|tool| allow(tool, &cwd_glob))
        .collect();
    rules.extend(PROJECT_READ_TOOLS.iter().map(|tool| allow(tool, &cwd_glob)));
    rules.extend(TRUSTED_UNSCOPED_TOOLS.iter().map(|tool| allow(tool, "*")));
    rules
}

/// Reading the project is what the read tools are already allowed to do without
/// asking, so a shell line that only observes and can only reach inside the
/// project earns the same treatment. Without this the classifier only kept plan
/// mode from refusing such a line; it still cost a prompt in either mode.
///
/// The rule turns entirely on `CONFINED_READ_ATTRIBUTE`, which the shell tool
/// sets only after judging both halves. An opaque line never carries it, and
/// `protected: Some(false)` refuses to cover one regardless.
fn builtin_structured_rules() -> Vec<PolicyRule> {
    vec![PolicyRule {
        origin: RuleOrigin::Builtin,
        rule: StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: WORKCELL_TOOL_OWNER.into(),
                contract: SHELL_EXECUTION_CONTRACT.into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Command,
                selector: PermissionResourceSelector::Any,
                access: Some(PermissionResourceAccess::Execute),
                protected: Some(false),
                attributes: BTreeMap::from([(
                    CONFINED_READ_ATTRIBUTE.into(),
                    PermissionResourceSelector::Exact {
                        value: CONFINED_READ_VALUE.into(),
                    },
                )]),
            }],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        },
    }]
}

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

#[derive(Debug, Error)]
pub struct PermissionError {
    tool: String,
    scope: String,
    guidance: Option<String>,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scope
        )?;
        if let Some(g) = &self.guidance {
            write!(f, " User guidance: {}", g)
        } else {
            write!(f, " {}", DEFAULT_DENY_GUIDANCE)
        }
    }
}

impl PermissionError {
    fn new(tool: &str, scope: &str) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: None,
        }
    }

    fn with_guidance(tool: &str, scope: &str, guidance: String) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: Some(guidance),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    AllowOnce,
    AllowSession,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    AllowOption {
        option_id: String,
        lifetime: PermissionLifetime,
    },
    /// One choice per resource of the request, composed into a single rule.
    /// A row left `None` is granted for this call only.
    AllowComposed {
        rows: Vec<Option<PermissionRowGrant>>,
        lifetime: PermissionLifetime,
    },
    Deny,
    DenyWithGuidance(String),
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionAnswer {
    pub fn decision_source(&self) -> &'static str {
        match self {
            Self::AllowOnce
            | Self::Deny
            | Self::DenyWithGuidance(_)
            | Self::AllowOption {
                lifetime: PermissionLifetime::Once,
                ..
            }
            | Self::AllowComposed {
                lifetime: PermissionLifetime::Once,
                ..
            } => DECISION_SOURCE_USER_ONCE,
            Self::AllowSession
            | Self::AllowOption {
                lifetime: PermissionLifetime::Conversation,
                ..
            }
            | Self::AllowComposed {
                lifetime: PermissionLifetime::Conversation,
                ..
            } => DECISION_SOURCE_USER_SESSION,
            Self::AllowAlwaysLocal
            | Self::AllowAlwaysGlobal
            | Self::DenyAlwaysLocal
            | Self::DenyAlwaysGlobal
            | Self::AllowOption { .. }
            | Self::AllowComposed { .. } => DECISION_SOURCE_USER_ALWAYS,
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            Self::AllowOnce
                | Self::AllowSession
                | Self::AllowAlwaysLocal
                | Self::AllowAlwaysGlobal
                | Self::AllowOption { .. }
                | Self::AllowComposed { .. }
        )
    }

    pub fn encode(&self) -> String {
        match self {
            Self::AllowOnce => "allow".to_string(),
            Self::AllowSession => "allow_session".to_string(),
            Self::AllowAlwaysLocal => "allow_always_local".to_string(),
            Self::AllowAlwaysGlobal => "allow_always_global".to_string(),
            Self::AllowOption {
                option_id,
                lifetime,
            } => format!("allow_option:{}:{option_id}", lifetime_name(lifetime)),
            Self::AllowComposed { rows, lifetime } => format!(
                "allow_composed:{}:{}",
                lifetime_name(lifetime),
                serde_json::to_string(rows).unwrap_or_default()
            ),
            Self::Deny => "deny".to_string(),
            Self::DenyWithGuidance(g) => format!("deny:{g}"),
            Self::DenyAlwaysLocal => "deny_always_local".to_string(),
            Self::DenyAlwaysGlobal => "deny_always_global".to_string(),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "allow_always_local" => Some(Self::AllowAlwaysLocal),
            "allow_always_global" => Some(Self::AllowAlwaysGlobal),
            "deny" => Some(Self::Deny),
            "deny_always_local" => Some(Self::DenyAlwaysLocal),
            "deny_always_global" => Some(Self::DenyAlwaysGlobal),
            _ if s.starts_with("allow_option:") => {
                let rest = s.strip_prefix("allow_option:")?;
                let (lifetime, option_id) = rest.split_once(':')?;
                Some(Self::AllowOption {
                    option_id: option_id.to_owned(),
                    lifetime: parse_lifetime(lifetime)?,
                })
            }
            _ if s.starts_with("allow_composed:") => {
                let rest = s.strip_prefix("allow_composed:")?;
                let (lifetime, rows) = rest.split_once(':')?;
                Some(Self::AllowComposed {
                    rows: serde_json::from_str(rows).ok()?,
                    lifetime: parse_lifetime(lifetime)?,
                })
            }
            _ if s.starts_with("deny:") => {
                let guidance = s.strip_prefix("deny:").unwrap();
                if guidance.is_empty() {
                    Some(Self::Deny)
                } else {
                    Some(Self::DenyWithGuidance(guidance.to_string()))
                }
            }
            _ => None,
        }
    }

    pub fn guidance(&self) -> Option<&str> {
        match self {
            Self::DenyWithGuidance(g) => Some(g),
            _ => None,
        }
    }
}

fn lifetime_name(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "once",
        PermissionLifetime::Conversation => "conversation",
        PermissionLifetime::Project => "project",
        PermissionLifetime::Global => "global",
    }
}

fn parse_lifetime(value: &str) -> Option<PermissionLifetime> {
    match value {
        "once" => Some(PermissionLifetime::Once),
        "conversation" => Some(PermissionLifetime::Conversation),
        "project" => Some(PermissionLifetime::Project),
        "global" => Some(PermissionLifetime::Global),
        _ => None,
    }
}

/// Permission rules declared by Lua plugins via
/// `caudra.api.register_permission_rule`, keyed by plugin name. Shared between
/// the Lua runtime (writer, on plugin load/unload) and every
/// [`PermissionManager`] (reader).
#[derive(Default)]
pub struct PluginRuleStore(Mutex<HashMap<Arc<str>, Vec<PermissionRule>>>);

impl PluginRuleStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, Vec<PermissionRule>>> {
        self.0.lock().unwrap_or_else(|e| {
            warn!("plugin rule mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    /// An empty `rules` removes the entry, so a reload that registers
    /// nothing clears the stale rules.
    pub fn replace(&self, plugin: &str, rules: Vec<PermissionRule>) {
        let mut map = self.lock();
        if rules.is_empty() {
            map.remove(plugin);
        } else {
            map.insert(Arc::from(plugin), rules);
        }
    }

    pub fn remove(&self, plugin: &str) {
        self.lock().remove(plugin);
    }

    pub fn snapshot(&self) -> Vec<PermissionRule> {
        self.lock().values().flatten().cloned().collect()
    }
}

pub struct PermissionManager {
    id: u64,
    structured_conversation_rules: Mutex<Vec<PermissionRuleRecord>>,
    conversation_policy_error: Mutex<Option<String>>,
    broker: Arc<PermissionBroker>,
    configured: RwLock<ConfiguredPolicy>,
    yolo: AtomicBool,
    /// Whether the user set yolo for this session themselves, which is what
    /// makes it worth persisting.
    yolo_explicit: AtomicBool,
    /// What `--yolo` / `always_yolo` seeded `yolo` with, so a session with no
    /// stored intent falls back to the flag instead of to off.
    seed_yolo: bool,
    project: Mutex<ProjectContext>,
    policy: Option<Arc<SharedPermissionState>>,
    plugin_rules: Arc<PluginRuleStore>,
}

#[derive(Clone)]
struct ConfiguredPolicy {
    rules: Vec<PermissionRule>,
    project_allow_rules: Vec<PermissionRule>,
    project_config_digest: Option<String>,
    project_config_root: Option<PathBuf>,
    review_candidates: Vec<PermissionReviewCandidate>,
    default: DefaultEffect,
    tool_defaults: HashMap<ToolKey, DefaultEffect>,
    remote_allow_rules: Vec<PermissionRule>,
    remote_allow_defaults: HashMap<ToolKey, DefaultEffect>,
    remote_default_allow: bool,
    remote_restrictive_rules: Vec<PermissionRule>,
    remote_restrictive_defaults: HashMap<ToolKey, DefaultEffect>,
    remote_restrictive_default: Option<DefaultEffect>,
    remote_policy_invalid: bool,
    remote_permission_asset: Option<(ProjectAssetTrustKey, String)>,
    remote_review_candidates: Vec<PermissionReviewCandidate>,
}

impl ConfiguredPolicy {
    fn clear_remote(&mut self, fail_closed: bool) {
        self.remote_restrictive_rules.clear();
        self.remote_restrictive_defaults.clear();
        self.remote_restrictive_default = fail_closed.then_some(DefaultEffect::Deny);
        self.remote_allow_rules.clear();
        self.remote_allow_defaults.clear();
        self.remote_default_allow = false;
        self.remote_review_candidates.clear();
        self.remote_permission_asset = None;
        self.remote_policy_invalid = fail_closed;
    }
}

#[derive(Clone)]
struct ProjectContext {
    cwd: PathBuf,
    canonical_project: Option<PathBuf>,
    policy_context_error: Option<String>,
    builtin_rules: Vec<PermissionRule>,
}

struct SharedPolicy {
    state: Option<PermissionState>,
    error: Option<String>,
}

#[derive(Default)]
struct PermissionBroker(Mutex<HashMap<u64, HashMap<String, PendingPermission>>>);

struct SharedPermissionState {
    state_dir: StateDir,
    policy: Mutex<SharedPolicy>,
    broker: Arc<PermissionBroker>,
}

/// What the rule set says about one request's resources.
///
/// `prompt_required` and `rule_ask` differ in exactly one case: a command no
/// authority covers yet that the builtin ask family names. That is not a rule
/// demanding review, it is the absence of one, so a later grant settles it.
struct RequestCoverage {
    /// Per resource: the authority that already allows it, if any.
    covered: Vec<Option<ResourceCoverage>>,
    must_prompt: bool,
    /// Per resource: an ask decision resolved it, covered or not.
    prompt_required: Vec<bool>,
    /// Per resource: a rule or a mode demands review whatever is granted later.
    rule_ask: Vec<bool>,
}

struct PendingPermission {
    request: PermissionRequest,
    /// Why a grant cannot settle this request on its own. A request nothing
    /// asked about is one a covering grant may sweep; `prompt_required` would
    /// also carry "not covered yet", which is the very thing the grant fixes.
    sticky_ask: Vec<bool>,
    project: Option<PathBuf>,
    event_tx: EventSender,
    sender: flume::Sender<PendingDecision>,
}

enum PendingDecision {
    Explicit(PermissionAnswer),
    MatchedRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokedRuleScope {
    Conversation,
    Project,
    Global,
}

/// One line of the configured, builtin, or plugin policy, for display in the
/// permissions picker. These rules are edited at their source rather than
/// revoked, so the picker shows them read-only.
#[derive(Clone)]
pub struct ActivePolicyRule {
    pub source: &'static str,
    pub rule: PermissionRule,
}

#[derive(Debug, Error)]
#[error("structured permission policy unavailable: {0}")]
pub struct PermissionPolicyError(String);

type SharedPolicies = HashMap<PathBuf, Weak<SharedPermissionState>>;

fn shared_policies() -> &'static Mutex<SharedPolicies> {
    static POLICIES: OnceLock<Mutex<SharedPolicies>> = OnceLock::new();
    POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shared_policy(state_dir: StateDir) -> Arc<SharedPermissionState> {
    let key =
        caudra_storage::paths::normalize_path(&state_dir.persistent_path().join(SESSIONS_DB_FILE));
    let mut policies = shared_policies().lock().unwrap_or_else(|error| {
        warn!("permission policy registry mutex was poisoned, recovering");
        error.into_inner()
    });
    if let Some(policy) = policies.get(&key).and_then(Weak::upgrade) {
        return policy;
    }
    policies.retain(|_, policy| policy.strong_count() > 0);
    let policy = Arc::new(SharedPermissionState {
        state_dir: state_dir.clone(),
        policy: Mutex::new(match PermissionState::open(&state_dir) {
            Ok(state) => SharedPolicy {
                state: Some(state),
                error: None,
            },
            Err(error) => SharedPolicy {
                state: None,
                error: Some(error.to_string()),
            },
        }),
        broker: Arc::default(),
    });
    policies.insert(key, Arc::downgrade(&policy));
    policy
}

impl SharedPolicy {
    fn state(&mut self) -> Result<&mut PermissionState, PermissionPolicyError> {
        if let Some(error) = &self.error {
            return Err(PermissionPolicyError(error.clone()));
        }
        let refresh = self
            .state
            .as_mut()
            .ok_or_else(|| PermissionPolicyError("state was not initialized".into()))?
            .refresh();
        if let Err(error) = refresh {
            let error = error.to_string();
            self.error = Some(error.clone());
            return Err(PermissionPolicyError(error));
        }
        self.state
            .as_mut()
            .ok_or_else(|| PermissionPolicyError("state was not initialized".into()))
    }
}

fn remove_pending(
    pending: &mut HashMap<u64, HashMap<String, PendingPermission>>,
    manager_id: u64,
    request_id: &str,
) -> Option<PendingPermission> {
    let requests = pending.get_mut(&manager_id)?;
    let removed = requests.remove(request_id);
    if requests.is_empty() {
        pending.remove(&manager_id);
    }
    removed
}

/// Whether a new rule settles everything a pending prompt was still waiting on.
///
/// A resource the candidate already had covered when it prompted does not need
/// saying again, so an answer narrowed to the part that was actually undecided
/// still dismisses the prompt. Without that, choosing to remember only the new
/// command in a batch would leave its siblings on screen forever.
///
/// A sticky ask is the rules themselves asking, which no grant answers.
fn rules_settle(rules: &[StructuredPermissionRule], candidate: &PendingPermission) -> bool {
    candidate.sticky_ask.len() == candidate.request.resources.len()
        && candidate.sticky_ask.iter().all(|sticky| !sticky)
        && candidate
            .request
            .resources
            .iter()
            .enumerate()
            .all(|(index, resource)| {
                rules
                    .iter()
                    .any(|rule| permission_rule_covers_resource(rule, &candidate.request, resource))
                    || candidate
                        .request
                        .presentation
                        .resources
                        .get(index)
                        .is_some_and(PermissionResourcePresentation::covered)
            })
}

fn reusable_rule_scope_matches(
    lifetime: &PermissionLifetime,
    source_manager_id: u64,
    source_project: Option<&Path>,
    candidate_manager_id: u64,
    candidate_project: Option<&Path>,
) -> bool {
    match lifetime {
        PermissionLifetime::Once => false,
        PermissionLifetime::Conversation => source_manager_id == candidate_manager_id,
        PermissionLifetime::Project => {
            source_project.is_some() && source_project == candidate_project
        }
        PermissionLifetime::Global => true,
    }
}

/// A stable numeric encoding of an effect for the project-config trust digest.
/// The numbers are part of the digest, so changing one re-prompts every trusted
/// project for consent it already gave.
fn config_effect_code(effect: Effect) -> u8 {
    match effect {
        Effect::Allow => 0,
        Effect::Ask => 1,
        Effect::Deny => 2,
    }
}

fn project_permission_config_digest(
    allow_rules: &[PermissionRule],
    restrictive_rules: &[PermissionRule],
) -> Option<String> {
    if allow_rules.is_empty() {
        return None;
    }
    let mut entries: Vec<_> = allow_rules
        .iter()
        .chain(restrictive_rules)
        .map(|rule| {
            (
                rule.tool.to_string(),
                rule.scope.as_deref().unwrap_or("*").to_owned(),
                config_effect_code(rule.effect),
            )
        })
        .collect();
    entries.sort();
    entries.dedup();

    let mut hasher = Sha256::new();
    hash_permission_config_field(&mut hasher, b"caudra-permission-config-v1");
    hash_permission_config_field(&mut hasher, &(entries.len() as u64).to_be_bytes());
    for (tool, pattern, effect) in entries {
        hash_permission_config_field(&mut hasher, tool.as_bytes());
        hash_permission_config_field(&mut hasher, pattern.as_bytes());
        hash_permission_config_field(&mut hasher, &[effect]);
    }
    Some(hex_encode(&hasher.finalize()))
}

fn hash_permission_config_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn configured_policy(
    config: PermissionsConfig,
    project_config_root: Option<PathBuf>,
) -> ConfiguredPolicy {
    let project_config_digest = project_permission_config_digest(
        &config.project_allow_rules,
        &config.project_restrictive_rules,
    );
    ConfiguredPolicy {
        rules: config.rules,
        project_allow_rules: config.project_allow_rules,
        project_config_digest,
        project_config_root,
        review_candidates: config.review_candidates,
        default: config.default,
        tool_defaults: config.tool_defaults,
        remote_allow_rules: Vec::new(),
        remote_allow_defaults: HashMap::new(),
        remote_default_allow: false,
        remote_restrictive_rules: Vec::new(),
        remote_restrictive_defaults: HashMap::new(),
        remote_restrictive_default: None,
        remote_policy_invalid: false,
        remote_permission_asset: None,
        remote_review_candidates: Vec::new(),
    }
}

/// The selector a configured scope stands for, read against the kind of the
/// resource it is being weighed against. A configured scope is one opaque
/// string whose meaning has always depended on the tool that owns it, so the
/// kind is what tells `cmd *` (a command pattern) from `dir *` (a prefix).
fn configured_selector(scope: &str, kind: &PermissionResourceKind) -> PermissionResourceSelector {
    if scope == "*" || scope == "**" {
        return PermissionResourceSelector::Any;
    }
    if let Some(root) = scope.strip_suffix(SUBTREE_SCOPE_SUFFIX) {
        return PermissionResourceSelector::Subtree {
            root: root.to_owned(),
        };
    }
    if *kind == PermissionResourceKind::Command && scope.ends_with(command_pattern::WILDCARD_SUFFIX)
    {
        return PermissionResourceSelector::CommandPattern {
            pattern: scope.to_owned(),
        };
    }
    match scope.strip_suffix('*') {
        Some(value) => PermissionResourceSelector::Prefix {
            value: value.to_owned(),
        },
        None => PermissionResourceSelector::Exact {
            value: scope.to_owned(),
        },
    }
}

/// Compiles one configured, builtin, or plugin rule into a rule the structured
/// evaluator can weigh, or `None` when it does not name this request's tool.
///
/// A configured rule names a `ToolKey` and an opaque scope. A structured rule
/// names a subject and typed resources. The tool key is answered here, by
/// selecting only the rules that name this request's tool, which lets the
/// compiled rule carry the request's own subject. Every remaining decision then
/// goes through `rule_standing`, so precedence has one definition.
fn compile_configured_rule(
    rule: &PermissionRule,
    request: &PermissionRequest,
) -> Option<StructuredPermissionRule> {
    if !command_rule_tool_matches(&rule.tool, &request.tool) {
        return None;
    }
    let effect = match rule.effect {
        Effect::Allow => StructuredPermissionEffect::Allow,
        Effect::Ask => StructuredPermissionEffect::Ask,
        Effect::Deny => StructuredPermissionEffect::Deny,
    };
    // Configured shell authority is granted to the shell tool, not to the name
    // `shell`. A plugin tool claiming the shell contract must not inherit it.
    // Restrictive policy is deliberately exempt: a deny has to reach the
    // impostor too.
    if effect == StructuredPermissionEffect::Allow
        && is_shell_tool(&request.tool)
        && !is_bound_shell_request(request)
    {
        return None;
    }
    // A configured allow never reaches a protected resource: the reviewed text
    // of a protected command or path says more than a scope in a file does. A
    // deny carries the opposite duty and is left free to match one, so it keeps
    // an unconstrained `protected`.
    let protected = (effect == StructuredPermissionEffect::Allow).then_some(false);
    Some(StructuredPermissionRule {
        subject: request.subject.clone(),
        executor: request.executor.clone(),
        resources: configured_constraints(rule.scope.as_deref(), protected, request),
        arguments: PermissionArgumentConstraint::Unconstrained,
        // Configured authority is granted outside the conversation, so plan
        // containment withholds it exactly as it withholds a stored project rule.
        lifetime: PermissionLifetime::Project,
        effect,
        family: None,
    })
}

/// One constraint per resource kind the request carries, because a configured
/// rule names no kind of its own.
///
/// An unscoped rule that has to pin `protected` still emits constraints rather
/// than an empty list, since an empty list is an unrestricted rule that no
/// constraint is ever consulted for.
fn configured_constraints(
    scope: Option<&str>,
    protected: Option<bool>,
    request: &PermissionRequest,
) -> Vec<PermissionResourceConstraint> {
    if scope.is_none() && protected.is_none() {
        return Vec::new();
    }
    let mut kinds: Vec<PermissionResourceKind> = Vec::new();
    for resource in &request.resources {
        if !kinds.contains(&resource.kind) {
            kinds.push(resource.kind.clone());
        }
    }
    kinds
        .into_iter()
        .flat_map(|kind| {
            let selector = scope.map_or(PermissionResourceSelector::Any, |scope| {
                configured_selector(scope, &kind)
            });
            // Restrictive policy also weighs the executable-name-resolved form,
            // so `/bin/rm -rf build` cannot dodge a `rm *` deny. The two
            // readings are separate constraints because a rule matches on any
            // one of its constraints while a constraint matches on all of its
            // attributes. Grants stay bound to the reviewed text.
            let normalized =
                (protected.is_none() && kind == PermissionResourceKind::Command).then(|| {
                    PermissionResourceConstraint {
                        kind: PermissionResourceKind::Command,
                        selector: PermissionResourceSelector::Any,
                        access: None,
                        protected: None,
                        attributes: BTreeMap::from([(
                            NORMALIZED_COMMAND_ATTRIBUTE.to_owned(),
                            selector.clone(),
                        )]),
                    }
                });
            [
                Some(PermissionResourceConstraint {
                    selector,
                    kind,
                    access: None,
                    protected,
                    attributes: BTreeMap::new(),
                }),
                normalized,
            ]
        })
        .flatten()
        .collect()
}

impl PermissionManager {
    /// Persistent production constructor. Managers for the same state path
    /// share live policy in-process and refresh disk before reads and writes.
    /// Atomic replacement protects readers and an owner-only sidecar lock
    /// serializes policy updates across processes.
    /// `new` remains an alias because the TUI prototype is constructed outside
    /// this crate.
    pub fn new(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
    ) -> Self {
        Self::new_persistent(config, cwd, plugin_rules)
    }

    pub fn new_persistent(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
    ) -> Self {
        match StateDir::resolve() {
            Ok(state_dir) => Self::new_persistent_in(config, cwd, plugin_rules, state_dir),
            Err(error) => Self::build(
                config,
                cwd,
                plugin_rules,
                None,
                Some(format!("cannot resolve state directory: {error}")),
            ),
        }
    }

    pub fn new_persistent_in(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
        state_dir: StateDir,
    ) -> Self {
        let canonical_project = std::fs::canonicalize(&cwd)
            .map_err(|error| format!("cannot canonicalize project {}: {error}", cwd.display()));
        let (canonical_project, context_error) = match canonical_project {
            Ok(project) => (Some(project), None),
            Err(error) => (None, Some(error)),
        };
        Self::build(
            config,
            cwd,
            plugin_rules,
            Some(shared_policy(state_dir)),
            context_error,
        )
        .with_canonical_project(canonical_project)
    }

    pub fn new_nonpersistent(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
    ) -> Self {
        Self::build(config, cwd, plugin_rules, None, None)
    }

    fn build(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
        policy: Option<Arc<SharedPermissionState>>,
        policy_context_error: Option<String>,
    ) -> Self {
        let seed_yolo = config.yolo;
        let configured = configured_policy(config, None);
        let builtin_rules = builtin_rules(&cwd);

        // Warn if wildcard deny is present — it blocks ALL tools including builtins.
        let has_wildcard_deny = configured
            .rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Deny);
        if has_wildcard_deny {
            warn!(
                "wildcard deny detected — this blocks ALL tools including \
                 builtins (write/edit/multiedit/task). Use per-tool rules \
                 instead if you want selective access."
            );
        }
        // Warn if wildcard allow is present — it permits ALL tools including write/edit/task.
        let has_wildcard_allow = configured
            .rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Allow);
        if has_wildcard_allow {
            warn!(
                "wildcard allow detected — this permits ALL tools including \
                 write/edit/multiedit/task. Use per-tool rules \
                 instead if you want selective access."
            );
        }

        Self {
            id: NEXT_PERMISSION_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            broker: policy
                .as_ref()
                .map(|state| Arc::clone(&state.broker))
                .unwrap_or_default(),
            configured: RwLock::new(configured),
            yolo: AtomicBool::new(seed_yolo),
            yolo_explicit: AtomicBool::new(false),
            seed_yolo,
            project: Mutex::new(ProjectContext {
                cwd,
                canonical_project: None,
                policy_context_error,
                builtin_rules,
            }),
            policy,
            plugin_rules,
        }
    }

    fn with_canonical_project(mut self, canonical_project: Option<PathBuf>) -> Self {
        self.configured
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .project_config_root = canonical_project.clone();
        self.project
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .canonical_project = canonical_project;
        self
    }

    fn project(&self) -> std::sync::MutexGuard<'_, ProjectContext> {
        self.project.lock().unwrap_or_else(|error| {
            warn!("permission project mutex was poisoned, recovering");
            error.into_inner()
        })
    }

    fn configured(&self) -> RwLockReadGuard<'_, ConfiguredPolicy> {
        self.configured.read().unwrap_or_else(|error| {
            warn!("permission config lock was poisoned, recovering");
            error.into_inner()
        })
    }

    /// Canonical session project root, independent of the process working directory.
    pub fn project_cwd(&self) -> PathBuf {
        let project = self.project();
        project
            .canonical_project
            .clone()
            .unwrap_or_else(|| caudra_storage::paths::canonicalize_clean(&project.cwd))
    }

    pub fn set_project(&self, cwd: &Path) {
        let canonical_project = std::fs::canonicalize(cwd)
            .map_err(|error| format!("cannot canonicalize project {}: {error}", cwd.display()));
        let (canonical_project, policy_context_error) = match canonical_project {
            Ok(project) => (Some(project), None),
            Err(error) => (None, Some(error)),
        };
        *self.project() = ProjectContext {
            cwd: cwd.to_path_buf(),
            canonical_project: canonical_project.clone(),
            policy_context_error,
            builtin_rules: builtin_rules(cwd),
        };
    }

    pub fn set_project_with_config(&self, cwd: &Path, config: PermissionsConfig) {
        let canonical_project = std::fs::canonicalize(cwd)
            .map_err(|error| format!("cannot canonicalize project {}: {error}", cwd.display()));
        let (canonical_project, policy_context_error) = match canonical_project {
            Ok(project) => (Some(project), None),
            Err(error) => (None, Some(error)),
        };
        let configured = configured_policy(config, canonical_project.clone());
        *self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner()) = configured;
        *self.project() = ProjectContext {
            cwd: cwd.to_path_buf(),
            canonical_project,
            policy_context_error,
            builtin_rules: builtin_rules(cwd),
        };
    }

    pub fn replace_remote_permission_asset(
        &self,
        asset: Option<&crate::remote_project_context::RemotePermissionAsset>,
    ) -> Result<(), PermissionPolicyError> {
        self.replace_remote_permission_asset_after(asset, || Ok(()))
            .map_err(|error| {
                self.invalidate_remote_permission_asset();
                PermissionPolicyError(error)
            })
    }

    pub fn replace_remote_permission_asset_after(
        &self,
        asset: Option<&crate::remote_project_context::RemotePermissionAsset>,
        before_install: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut current = self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut configured = current.clone();
        configured.clear_remote(asset.is_some());
        let Some(asset) = asset else {
            before_install()?;
            *current = configured;
            return Ok(());
        };
        let trust_key = asset
            .source
            .trust_key()
            .map_err(|error| error.to_string())?;
        configured.remote_restrictive_rules = asset.declarations.restrictive_rules.clone();
        configured.remote_restrictive_defaults = asset.declarations.restrictive_defaults.clone();
        configured.remote_restrictive_default = asset
            .declarations
            .default
            .filter(|effect| *effect != DefaultEffect::Allow);
        configured.remote_allow_rules = asset.declarations.allow_rules.clone();
        configured.remote_allow_defaults = asset.declarations.allow_defaults.clone();
        configured.remote_default_allow = asset.declarations.default == Some(DefaultEffect::Allow);
        configured.remote_review_candidates = asset
            .declarations
            .allow_rules
            .iter()
            .map(|rule| PermissionReviewCandidate {
                source: caudra_config::PermissionSource::Project,
                kind: caudra_config::PermissionReviewKind::Rule,
                tool: Some(rule.tool.clone()),
                scope: rule.scope.clone(),
            })
            .chain(
                asset
                    .declarations
                    .allow_defaults
                    .keys()
                    .cloned()
                    .map(|tool| PermissionReviewCandidate {
                        source: caudra_config::PermissionSource::Project,
                        kind: caudra_config::PermissionReviewKind::Default,
                        tool: Some(tool),
                        scope: None,
                    }),
            )
            .chain(
                configured
                    .remote_default_allow
                    .then_some(PermissionReviewCandidate {
                        source: caudra_config::PermissionSource::Project,
                        kind: caudra_config::PermissionReviewKind::Default,
                        tool: None,
                        scope: None,
                    }),
            )
            .collect();
        configured.remote_permission_asset = (!configured.remote_review_candidates.is_empty())
            .then(|| (trust_key, asset.digest.clone()));
        configured.remote_policy_invalid = false;
        before_install()?;
        *current = configured;
        Ok(())
    }

    pub fn invalidate_remote_permission_asset(&self) {
        let mut configured = self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner());
        configured.clear_remote(true);
    }

    fn remote_restrictive_default(&self, tool: &ToolKey) -> Option<DefaultEffect> {
        let configured = self.configured();
        configured
            .remote_restrictive_defaults
            .get(tool)
            .copied()
            .or_else(|| match tool {
                ToolKey::McpTool { server, .. } => configured
                    .remote_restrictive_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied(),
                _ => None,
            })
            .or(configured.remote_restrictive_default)
    }

    fn remote_default_denies(&self, tool: &ToolKey, request: &PermissionRequest) -> bool {
        if self.remote_restrictive_default(tool) != Some(DefaultEffect::Deny) {
            return false;
        }
        let configured = self.configured();
        if !self.remote_allows_active(&configured) {
            return true;
        }
        let default_allows = configured
            .remote_allow_defaults
            .get(tool)
            .copied()
            .or_else(|| match tool {
                ToolKey::McpTool { server, .. } => configured
                    .remote_allow_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied(),
                _ => None,
            })
            == Some(DefaultEffect::Allow)
            || configured.remote_default_allow;
        if default_allows {
            false
        } else {
            !permission_rules_cover_request(
                &configured
                    .remote_allow_rules
                    .iter()
                    .filter_map(|rule| compile_configured_rule(rule, request))
                    .collect::<Vec<_>>(),
                request,
            )
        }
    }

    /// Fresh manager for a new session runtime: shares config and builtin
    /// rules plus the current yolo state, but owns empty session rules so
    /// restoring one session never clobbers another's grants.
    pub fn fork(&self) -> Self {
        let project = self.project().clone();
        let configured = self.configured().clone();
        Self {
            id: NEXT_PERMISSION_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            broker: Arc::clone(&self.broker),
            configured: RwLock::new(configured),
            yolo: AtomicBool::new(self.is_yolo()),
            yolo_explicit: AtomicBool::new(self.yolo_explicit.load(Ordering::Relaxed)),
            seed_yolo: self.seed_yolo,
            project: Mutex::new(project),
            policy: self.policy.clone(),
            plugin_rules: Arc::clone(&self.plugin_rules),
        }
    }

    fn structured_conversation_rules(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<PermissionRuleRecord>> {
        self.structured_conversation_rules
            .lock()
            .unwrap_or_else(|error| {
                warn!("structured permission mutex was poisoned, recovering");
                error.into_inner()
            })
    }

    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, HashMap<String, PendingPermission>>> {
        self.broker.0.lock().unwrap_or_else(|error| {
            warn!("permission request mutex was poisoned, recovering");
            error.into_inner()
        })
    }

    pub fn answer(&self, request_id: &str, answer: PermissionAnswer) -> bool {
        let mut pending = self.pending();
        let Some((request, project)) = pending
            .get(&self.id)
            .and_then(|requests| requests.get(request_id))
            .map(|pending| (pending.request.clone(), pending.project.clone()))
        else {
            return false;
        };
        let reusable_rules =
            match self.commit_structured_decision(&request, &answer, project.as_deref()) {
                Ok(rules) => rules,
                Err(error) => {
                    warn!(%error, request_id, "permission decision was not committed");
                    return false;
                }
            };
        let Some(answered) = remove_pending(&mut pending, self.id, request_id) else {
            return false;
        };
        let mut covered = Vec::new();
        // One answer files at one lifetime, so scope is settled once for the set.
        if let Some(lifetime) = reusable_rules.first().map(|rule| rule.lifetime.clone()) {
            let mut matches = Vec::new();
            for (&manager_id, requests) in pending.iter() {
                for (candidate_id, candidate) in requests {
                    if reusable_rule_scope_matches(
                        &lifetime,
                        self.id,
                        project.as_deref(),
                        manager_id,
                        candidate.project.as_deref(),
                    ) && rules_settle(&reusable_rules, candidate)
                    {
                        matches.push((manager_id, candidate_id.clone()));
                    }
                }
            }
            for (manager_id, candidate_id) in matches {
                if let Some(candidate) = remove_pending(&mut pending, manager_id, &candidate_id) {
                    covered.push(candidate);
                }
            }
        }
        drop(pending);

        for candidate in covered {
            if candidate
                .event_tx
                .send(AgentEvent::PermissionRequestResolved {
                    request_id: candidate.request.id.clone(),
                    source_request_id: request_id.to_owned(),
                })
                .is_err()
            {
                warn!(request_id = %candidate.request.id, "swept permission prompt was not dismissed");
            }
            if candidate
                .sender
                .try_send(PendingDecision::MatchedRule)
                .is_err()
            {
                warn!(request_id = %candidate.request.id, "permission requester already closed");
            }
        }
        if answered
            .sender
            .try_send(PendingDecision::Explicit(answer))
            .is_err()
        {
            warn!(request_id, "permission requester already closed");
        }
        true
    }

    pub fn pending_count(&self) -> usize {
        self.pending().get(&self.id).map_or(0, HashMap::len)
    }

    pub fn pending_request(&self, request_id: &str) -> Option<PermissionRequest> {
        self.pending()
            .get(&self.id)
            .and_then(|requests| requests.get(request_id))
            .map(|pending| pending.request.clone())
    }

    fn remove_pending(&self, request_id: &str) {
        remove_pending(&mut self.pending(), self.id, request_id);
    }

    /// The builtin allowlist of fully literal, side-effect-free commands. It is
    /// a default rather than a rule, so it is consulted only where no rule
    /// speaks: ranking it against configured policy would let `echo hi` outrank
    /// a configured `echo *` ask purely for being the more exact text.
    fn builtin_command_allow(resource: &PermissionResource) -> Option<ResourceCoverage> {
        (resource.kind == PermissionResourceKind::Command && !resource.protected)
            .then(|| command_pattern::builtin_allow_pattern(&resource.value))
            .flatten()
            .map(|pattern| ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: pattern.to_owned(),
            })
    }

    fn builtin_command_ask(resource: &PermissionResource) -> bool {
        resource.kind == PermissionResourceKind::Command
            && command_pattern::BUILTIN_ASK_PATTERNS.iter().any(|pattern| {
                command_pattern::matches(pattern, &resource.value)
                    || resource
                        .attributes
                        .get(NORMALIZED_COMMAND_ATTRIBUTE)
                        .is_some_and(|normalized| command_pattern::matches(pattern, normalized))
            })
    }

    fn request_coverage(
        &self,
        request: &PermissionRequest,
        structured_rules: &[PolicyRule],
        builtin_allows: bool,
    ) -> RequestCoverage {
        let mut must_prompt = false;
        let mut prompt_required = vec![false; request.resources.len()];
        let mut rule_ask = vec![false; request.resources.len()];
        let covered = request
            .resources
            .iter()
            .enumerate()
            .map(|(index, resource)| {
                let standing =
                    permission_rules_resource_standing(structured_rules, request, resource);
                match standing.decision {
                    StructuredPermissionDecision::Allow => standing.coverage,
                    // An ask withholds authority without erasing it. The
                    // resource stays covered so the prompt can say so and a
                    // later grant can sweep it.
                    StructuredPermissionDecision::Ask => {
                        must_prompt = true;
                        prompt_required[index] = true;
                        rule_ask[index] = true;
                        standing.coverage
                    }
                    StructuredPermissionDecision::Deny => None,
                    StructuredPermissionDecision::NoMatch => {
                        let covered = builtin_allows
                            .then(|| Self::builtin_command_allow(resource))
                            .flatten();
                        if covered.is_none() && Self::builtin_command_ask(resource) {
                            must_prompt = true;
                            prompt_required[index] = true;
                        }
                        covered
                    }
                }
            })
            .collect();
        RequestCoverage {
            covered,
            must_prompt,
            prompt_required,
            rule_ask,
        }
    }

    fn default_effect(&self, tool: &ToolKey) -> DefaultEffect {
        if let Some(effect) = self.remote_restrictive_default(tool) {
            return effect;
        }
        let configured = self.configured();
        let remote_allows_active = self.remote_allows_active(&configured);
        let effect = configured
            .tool_defaults
            .get(tool)
            .copied()
            .or_else(|| {
                let server = match tool {
                    ToolKey::McpTool { server, .. } => server,
                    _ => return None,
                };
                configured
                    .tool_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied()
            })
            .unwrap_or(configured.default);
        if effect != DefaultEffect::Prompt || !remote_allows_active {
            return effect;
        }
        configured
            .remote_allow_defaults
            .get(tool)
            .copied()
            .or_else(|| match tool {
                ToolKey::McpTool { server, .. } => configured
                    .remote_allow_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied(),
                _ => None,
            })
            .unwrap_or_else(|| {
                if configured.remote_default_allow {
                    DefaultEffect::Allow
                } else {
                    effect
                }
            })
    }

    /// The explicit toggle, so it also claims the session's intent: `/yolo` off
    /// under `--yolo` genuinely turns the session off and is remembered.
    pub fn toggle_yolo(&self) -> bool {
        let enabled = !self.yolo.fetch_xor(true, Ordering::Relaxed);
        self.yolo_explicit.store(true, Ordering::Relaxed);
        enabled
    }

    /// Replaces whatever this session was running with: `Some` is the user's
    /// stored intent, `None` means they never expressed one and the seed
    /// applies again.
    pub fn set_session_yolo(&self, stored: Option<bool>) {
        self.yolo
            .store(stored.unwrap_or(self.seed_yolo), Ordering::Relaxed);
        self.yolo_explicit
            .store(stored.is_some(), Ordering::Relaxed);
    }

    pub fn is_yolo(&self) -> bool {
        self.yolo.load(Ordering::Relaxed)
    }

    /// What the session may persist. A one-shot `--yolo` is a property of the
    /// invocation, so on its own it stores nothing.
    pub fn persisted_yolo(&self) -> Option<bool> {
        self.yolo_explicit
            .load(Ordering::Relaxed)
            .then(|| self.is_yolo())
    }

    /// Outside-cwd paths are not blocked here. They flow through the normal
    /// permission prompt (which uses the same canonicalization via
    /// [`scope_matches`]). Only unresolvable boundaries are hard-blocked.
    pub fn boundary_block_reason(&self, path: &Path) -> Option<String> {
        match physical_boundary_check(&self.project().cwd, path) {
            Some(_) => None,
            None => Some(format!(
                "{BOUNDARY_UNVERIFIABLE_PREFIX} {} \
                 (project root could not be resolved)",
                path.display()
            )),
        }
    }

    pub fn structured_conversation_rules_snapshot(&self) -> Vec<PermissionRuleRecord> {
        self.structured_conversation_rules().clone()
    }

    pub fn load_structured_conversation_rules(&self, rules: Vec<PermissionRuleRecord>) {
        let error = rules.iter().find_map(|rule| {
            validate_conversation_record(rule).err().map(|error| {
                warn!(%error, rule_id = %rule.id, "conversation permission policy failed closed");
                error.to_string()
            })
        });
        *self
            .conversation_policy_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = error;
        *self.structured_conversation_rules() = rules;
    }

    fn ensure_conversation_policy_valid(&self) -> Result<(), PermissionPolicyError> {
        if let Some(error) = self
            .conversation_policy_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            Err(PermissionPolicyError(error.clone()))
        } else {
            Ok(())
        }
    }

    pub fn structured_rule_inventory(
        &self,
    ) -> Result<Vec<PermissionRuleRecord>, PermissionPolicyError> {
        self.ensure_conversation_policy_valid()?;
        let mut inventory: Vec<_> = self
            .structured_conversation_rules()
            .iter()
            .filter(|record| record.is_active())
            .cloned()
            .collect();
        inventory.extend(self.persistent_records()?);
        inventory.sort_by_key(|record| record.created_at);
        Ok(inventory)
    }

    fn project_allows_active(&self) -> bool {
        let canonical_project = self.project().canonical_project.clone();
        let configured = self.configured();
        configured
            .project_config_digest
            .as_deref()
            .zip(configured.project_config_root.as_deref())
            .zip(canonical_project.as_deref())
            .zip(self.policy.as_ref())
            .is_some_and(|(((digest, config_root), project), policy)| {
                config_root == project
                    && is_permission_config_trusted(&policy.state_dir, project, digest)
                        .unwrap_or_else(|error| {
                            warn!(%error, "could not read project permission config trust");
                            false
                        })
            })
    }

    fn remote_allows_active(&self, configured: &ConfiguredPolicy) -> bool {
        configured
            .remote_permission_asset
            .as_ref()
            .zip(self.policy.as_ref())
            .is_some_and(|((asset, digest), policy)| {
                is_remote_asset_trusted(&policy.state_dir, asset, digest).unwrap_or_else(|error| {
                    warn!(%error, "could not read remote permission config trust");
                    false
                })
            })
    }

    fn active_config_rules(&self) -> Vec<PermissionRule> {
        let project_allows_active = self.project_allows_active();
        let configured = self.configured();
        let remote_allows_active = self.remote_allows_active(&configured);
        configured
            .rules
            .iter()
            .chain(configured.remote_restrictive_rules.iter())
            .chain(
                configured
                    .project_allow_rules
                    .iter()
                    .filter(move |_| project_allows_active),
            )
            .chain(
                configured
                    .remote_allow_rules
                    .iter()
                    .filter(move |_| remote_allows_active),
            )
            .cloned()
            .collect()
    }

    pub fn needs_project_permission_config_trust(&self) -> bool {
        let project_allows_active = self.project_allows_active();
        let configured = self.configured();
        let remote_allows_active = self.remote_allows_active(&configured);
        (configured.project_config_digest.is_some()
            && configured.project_config_root.is_some()
            && !project_allows_active
            && self.project().canonical_project.as_ref() == configured.project_config_root.as_ref())
            || (configured.remote_permission_asset.is_some() && !remote_allows_active)
    }

    pub fn project_permission_config_trusted(&self) -> bool {
        let project_allows_active = self.project_allows_active();
        let configured = self.configured();
        let remote_allows_active = self.remote_allows_active(&configured);
        (configured.project_config_digest.is_some()
            && configured.project_config_root.is_some()
            && self.project().canonical_project.as_ref() == configured.project_config_root.as_ref()
            && project_allows_active)
            || (configured.remote_permission_asset.is_some() && remote_allows_active)
    }

    pub fn trust_project_permission_config(&self) -> Result<(), PermissionPolicyError> {
        let configured = self.configured();
        if let Some((asset, digest)) = &configured.remote_permission_asset {
            let policy = self
                .policy
                .as_ref()
                .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
            return trust_remote_asset(&policy.state_dir, asset, digest)
                .map_err(|error| PermissionPolicyError(error.to_string()));
        }
        let digest = configured
            .project_config_digest
            .clone()
            .ok_or_else(|| PermissionPolicyError("project config has no allow rules".into()))?;
        let project_config_root = configured.project_config_root.clone();
        drop(configured);
        let project = self.project();
        let canonical_project = project
            .canonical_project
            .as_deref()
            .filter(|project| Some(*project) == project_config_root.as_deref())
            .ok_or_else(|| {
                PermissionPolicyError("project permission config is not active here".into())
            })?;
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
        trust_project_permission_config(&policy.state_dir, canonical_project, &digest)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        Ok(())
    }

    pub fn revoke_project_permission_config_trust(&self) -> Result<(), PermissionPolicyError> {
        if let Some((asset, _)) = &self.configured().remote_permission_asset {
            let policy = self
                .policy
                .as_ref()
                .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
            return revoke_remote_asset_trust(&policy.state_dir, asset)
                .map_err(|error| PermissionPolicyError(error.to_string()));
        }
        let project_config_root = self.configured().project_config_root.clone();
        let project = self.project();
        let canonical_project = project
            .canonical_project
            .as_deref()
            .filter(|project| Some(*project) == project_config_root.as_deref())
            .ok_or_else(|| {
                PermissionPolicyError("project permission config is not active here".into())
            })?;
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
        revoke_project_permission_config(&policy.state_dir, canonical_project)
            .map_err(|error| PermissionPolicyError(error.to_string()))
    }

    pub fn review_candidates(&self) -> Vec<PermissionReviewCandidate> {
        let project_allows_active = self.project_allows_active();
        let configured = self.configured();
        let remote_allows_active = self.remote_allows_active(&configured);
        let mut candidates = configured.review_candidates.clone();
        if !remote_allows_active {
            candidates.extend(configured.remote_review_candidates.iter().cloned());
        }
        if project_allows_active {
            candidates.retain(|candidate| {
                candidate.source != caudra_config::PermissionSource::Project
                    || candidate.kind != caudra_config::PermissionReviewKind::Rule
                    || !configured.project_allow_rules.iter().any(|rule| {
                        candidate.tool.as_ref() == Some(&rule.tool) && candidate.scope == rule.scope
                    })
            });
        }
        candidates
    }

    /// The policy the picker shows but cannot revoke: `permissions.toml`, the
    /// builtin project allows, and rules registered by trusted plugins.
    pub fn active_policy(&self) -> Vec<ActivePolicyRule> {
        let builtin_rules = self.project().builtin_rules.clone();
        let mut entries: Vec<_> = self
            .active_config_rules()
            .into_iter()
            .map(|rule| ActivePolicyRule {
                source: "configuration",
                rule,
            })
            .collect();
        entries.extend(builtin_rules.iter().cloned().map(|rule| ActivePolicyRule {
            source: "builtin",
            rule,
        }));
        entries.extend(
            self.plugin_rules
                .snapshot()
                .into_iter()
                .map(|rule| ActivePolicyRule {
                    source: "trusted plugin",
                    rule,
                }),
        );
        entries
    }

    pub fn revoke_structured_rule(
        &self,
        id: &str,
    ) -> Result<Option<RevokedRuleScope>, PermissionPolicyError> {
        self.ensure_conversation_policy_valid()?;
        {
            let mut conversation = self.structured_conversation_rules();
            if let Some(record) = conversation
                .iter_mut()
                .find(|record| record.id == id && record.is_active())
            {
                record.revoked_at = Some(now_epoch());
                return Ok(Some(RevokedRuleScope::Conversation));
            }
        }

        let applicable = self.persistent_records()?;
        let Some(record) = applicable.iter().find(|record| record.id == id) else {
            return Ok(None);
        };
        let scope = match record.rule.lifetime {
            PermissionLifetime::Project => RevokedRuleScope::Project,
            PermissionLifetime::Global => RevokedRuleScope::Global,
            PermissionLifetime::Once | PermissionLifetime::Conversation => return Ok(None),
        };
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
        let mut policy = policy.policy.lock().unwrap_or_else(|error| {
            warn!("permission policy mutex was poisoned, recovering");
            error.into_inner()
        });
        let state = policy.state()?;
        if state
            .revoke(id)
            .map_err(|error| PermissionPolicyError(error.to_string()))?
        {
            Ok(Some(scope))
        } else {
            Err(PermissionPolicyError(
                "rule changed before revocation".into(),
            ))
        }
    }

    /// The configured, builtin, and plugin policy compiled against this
    /// request, so one evaluator weighs it alongside the stored rules.
    fn configured_structured_rules(
        &self,
        request: &PermissionRequest,
        include_builtin_allows: bool,
    ) -> Vec<PolicyRule> {
        let config = self.active_config_rules();
        let builtin = self.project().builtin_rules.clone();
        let plugin = self.plugin_rules.snapshot();
        let tagged = |origin| move |rule| (origin, rule);
        config
            .iter()
            .map(tagged(RuleOrigin::Config))
            .chain(
                builtin
                    .iter()
                    .filter(|_| include_builtin_allows)
                    .map(tagged(RuleOrigin::Builtin)),
            )
            .chain(plugin.iter().map(tagged(RuleOrigin::Plugin)))
            .filter_map(|(origin, rule)| {
                compile_configured_rule(rule, request).map(|rule| PolicyRule { origin, rule })
            })
            .collect()
    }

    /// The rules that may speak to a call, contained to the plan when one is
    /// being built: an authority granted before the plan is not one the plan
    /// asked for, so no persistent allow applies. Configured policy carries a
    /// `Project` lifetime for exactly this reason. Denials and asks are left
    /// alone, because containment narrows and must never widen.
    fn applicable_rules_within(
        &self,
        request: &PermissionRequest,
        plan_scoped: bool,
        include_builtin_allows: bool,
    ) -> Result<Vec<PolicyRule>, PermissionPolicyError> {
        let mut rules = self.applicable_structured_rules()?;
        rules.extend(self.configured_structured_rules(request, include_builtin_allows));
        if plan_scoped {
            rules.retain(|policy| {
                policy.rule.effect != StructuredPermissionEffect::Allow
                    || matches!(
                        policy.rule.lifetime,
                        PermissionLifetime::Once | PermissionLifetime::Conversation
                    )
            });
        }
        Ok(rules)
    }

    fn applicable_structured_rules(&self) -> Result<Vec<PolicyRule>, PermissionPolicyError> {
        self.ensure_conversation_policy_valid()?;
        let mut rules = builtin_structured_rules();
        rules.extend(
            self.structured_conversation_rules()
                .iter()
                .filter(|record| record.is_active())
                .map(|record| PolicyRule {
                    origin: RuleOrigin::Conversation,
                    rule: record.rule.clone(),
                }),
        );
        rules.extend(
            self.persistent_records()?
                .into_iter()
                .map(|record| PolicyRule {
                    origin: match record.rule.lifetime {
                        PermissionLifetime::Global => RuleOrigin::Global,
                        _ => RuleOrigin::Project,
                    },
                    rule: record.rule,
                }),
        );
        Ok(rules)
    }

    fn persistent_records(&self) -> Result<Vec<PermissionRuleRecord>, PermissionPolicyError> {
        let project_context = self.project();
        let Some(policy) = &self.policy else {
            if let Some(error) = &project_context.policy_context_error {
                return Err(PermissionPolicyError(error.clone()));
            }
            return Ok(Vec::new());
        };
        let project = project_context.canonical_project.clone().ok_or_else(|| {
            PermissionPolicyError(
                project_context
                    .policy_context_error
                    .clone()
                    .unwrap_or_else(|| "canonical project is unavailable".into()),
            )
        })?;
        drop(project_context);
        let mut policy = policy.policy.lock().unwrap_or_else(|error| {
            warn!("permission policy mutex was poisoned, recovering");
            error.into_inner()
        });
        Ok(policy
            .state()?
            .records()
            .iter()
            .filter(|record| {
                record.is_active()
                    && match record.rule.lifetime {
                        PermissionLifetime::Global => record.project.is_none(),
                        PermissionLifetime::Project => record.project.as_ref() == Some(&project),
                        PermissionLifetime::Once | PermissionLifetime::Conversation => false,
                    }
            })
            .cloned()
            .collect())
    }

    /// Files what the answer decided and reports the allows other pending
    /// prompts can be swept with. A composed answer files one rule per command,
    /// so each is listed and revoked on its own.
    fn commit_structured_decision(
        &self,
        request: &PermissionRequest,
        answer: &PermissionAnswer,
        approved_project: Option<&Path>,
    ) -> Result<Vec<StructuredPermissionRule>, PermissionPolicyError> {
        let (option_id, lifetime) = match answer {
            PermissionAnswer::AllowOnce => ("allow_exact", PermissionLifetime::Once),
            PermissionAnswer::AllowSession => ("allow_exact", PermissionLifetime::Conversation),
            PermissionAnswer::AllowAlwaysLocal => ("allow_exact", PermissionLifetime::Project),
            PermissionAnswer::AllowAlwaysGlobal => ("allow_exact", PermissionLifetime::Global),
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => (option_id.as_str(), lifetime.clone()),
            PermissionAnswer::AllowComposed { rows, lifetime } => {
                let mut stored = Vec::new();
                for rule in request
                    .composed_rules(rows, lifetime)
                    .map_err(|error| PermissionPolicyError(error.to_string()))?
                {
                    stored.extend(self.store_reusable_rule(request, rule, approved_project)?);
                }
                return Ok(stored);
            }
            PermissionAnswer::DenyAlwaysLocal => ("deny_exact", PermissionLifetime::Project),
            PermissionAnswer::DenyAlwaysGlobal => ("deny_exact", PermissionLifetime::Global),
            PermissionAnswer::Deny | PermissionAnswer::DenyWithGuidance(_) => {
                return Ok(Vec::new());
            }
        };
        let option = request
            .options
            .iter()
            .find(|option| option.id == option_id)
            .ok_or_else(|| {
                PermissionPolicyError(format!("request did not offer {option_id:?} authority"))
            })?;
        if !option.allowed_lifetimes.contains(&lifetime) {
            return Err(PermissionPolicyError(format!(
                "authority {option_id:?} does not allow {lifetime:?} lifetime"
            )));
        }
        let mut rule = option.rule.clone();
        rule.lifetime = lifetime;
        let covers = match rule.effect {
            StructuredPermissionEffect::Allow => permission_rule_covers_request(&rule, request),
            StructuredPermissionEffect::Deny | StructuredPermissionEffect::Ask => {
                permission_rule_intersects_request(&rule, request)
            }
        };
        if !covers {
            return Err(PermissionPolicyError(format!(
                "authority {option_id:?} does not cover the pending request"
            )));
        }
        Ok(Vec::from_iter(self.store_reusable_rule(
            request,
            rule,
            approved_project,
        )?))
    }

    /// Files a validated rule wherever its lifetime belongs, and reports the
    /// allow that other pending prompts can be swept with.
    fn store_reusable_rule(
        &self,
        request: &PermissionRequest,
        rule: StructuredPermissionRule,
        approved_project: Option<&Path>,
    ) -> Result<Option<StructuredPermissionRule>, PermissionPolicyError> {
        if rule.lifetime == PermissionLifetime::Once {
            return Ok(None);
        }
        let reusable_allow =
            (rule.effect == StructuredPermissionEffect::Allow).then(|| rule.clone());
        let review = Some(redacted_review_shape(&request.input));
        if rule.lifetime == PermissionLifetime::Conversation {
            let record = PermissionRuleRecord::conversation_with_review(rule, review)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            self.structured_conversation_rules().push(record);
            return Ok(reusable_allow);
        }

        let project = match rule.lifetime {
            PermissionLifetime::Project => {
                Some(approved_project.map(Path::to_path_buf).ok_or_else(|| {
                    PermissionPolicyError("canonical project is unavailable".into())
                })?)
            }
            PermissionLifetime::Global => None,
            PermissionLifetime::Once | PermissionLifetime::Conversation => {
                return Err(PermissionPolicyError(
                    "request offered a non-persistent rule".into(),
                ));
            }
        };
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
        let mut policy = policy.policy.lock().unwrap_or_else(|error| {
            warn!("permission policy mutex was poisoned, recovering");
            error.into_inner()
        });
        policy
            .state()?
            .insert_with_review(project, rule, review)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        Ok(reusable_allow)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
    ) -> Result<(), PermissionError> {
        self.enforce_with_identity(
            tool,
            scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            None,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce_with_identity(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
    ) -> Result<(), PermissionError> {
        self.enforce_inner(
            tool,
            scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            identity,
            include_builtin_allows,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce_with_intent(
        &self,
        tool: &ToolKey,
        intent: &crate::tools::PermissionIntent,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
    ) -> Result<(), PermissionError> {
        self.enforce_inner(
            tool,
            &intent.scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            identity,
            include_builtin_allows,
            Some(intent),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn enforce_inner(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
        intent: Option<&crate::tools::PermissionIntent>,
    ) -> Result<(), PermissionError> {
        // A plan-scoped call is unreviewable in the same way a forced prompt
        // is, so it is built and presented the same way. The difference is what
        // may settle it, which is decided against the rules, not here.
        let unreviewed = scopes.force_prompt || scopes.plan_scoped;
        let (cwd, canonical_project) = {
            let project = self.project();
            (project.cwd.clone(), project.canonical_project.clone())
        };
        let tool_string = tool.to_string();
        let scope_display = || scopes.scopes.join("; ");
        // Every deny is built here and every approval passes through
        // `allowed`, so reporting cannot drift from what the caller gets.
        let deny = |source: &'static str, guidance: Option<String>| {
            caudra_otel::emit::tool_decision(
                &tool_string,
                caudra_otel::emit::DECISION_REJECT,
                source,
            );
            match guidance {
                Some(g) => PermissionError::with_guidance(&tool_string, &scope_display(), g),
                None => PermissionError::new(&tool_string, &scope_display()),
            }
        };
        let allowed = |source: &'static str| {
            caudra_otel::emit::tool_decision(
                &tool_string,
                caudra_otel::emit::DECISION_ACCEPT,
                source,
            );
            Ok(())
        };
        let by_rule = || {
            if self.yolo.load(Ordering::Relaxed) {
                DECISION_SOURCE_YOLO
            } else {
                DECISION_SOURCE_RULE
            }
        };

        let make_request = |tool: ToolKey, request_scopes: Vec<String>, force_prompt: bool| {
            if let Some(intent) = intent {
                let mut intent = intent.clone();
                intent.scopes.force_prompt = force_prompt;
                return match &identity {
                    Some((subject, executor)) => PermissionRequest::from_intent_with_identity(
                        request_id.to_owned(),
                        tool,
                        &intent,
                        input.clone(),
                        &cwd,
                        subject.clone(),
                        executor.clone(),
                    ),
                    None => PermissionRequest::from_intent(
                        request_id.to_owned(),
                        tool,
                        &intent,
                        input.clone(),
                        &cwd,
                    ),
                };
            }
            match &identity {
                Some((subject, executor)) => PermissionRequest::from_legacy_with_identity(
                    request_id.to_owned(),
                    tool,
                    request_scopes,
                    input.clone(),
                    &cwd,
                    force_prompt,
                    subject.clone(),
                    executor.clone(),
                ),
                None => PermissionRequest::from_legacy(
                    request_id.to_owned(),
                    tool,
                    request_scopes,
                    input.clone(),
                    &cwd,
                    force_prompt,
                ),
            }
        };
        let initial_request = make_request(tool.clone(), scopes.scopes.clone(), unreviewed);
        let exact_plan_write = plan_path.is_some_and(|plan_path| {
            matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
                && !initial_request.resources.is_empty()
                && initial_request.resources.iter().all(|resource| {
                    resource.access == Some(PermissionResourceAccess::Write)
                        && normalize_scope_path(&resource.value)
                            == normalize_scope_path(&plan_path.display().to_string())
                })
        });
        let force_prompt = unreviewed
            || (!exact_plan_write
                && initial_request
                    .resources
                    .iter()
                    .any(|resource| resource.requires_prompt));
        let full_request = if force_prompt == unreviewed {
            initial_request
        } else {
            make_request(tool.clone(), scopes.scopes.clone(), force_prompt)
        };
        if intent.is_some() && full_request.resources.is_empty() {
            warn!(tool = %tool, "explicit permission intent has no resources");
            return Err(deny(
                DECISION_SOURCE_RULE,
                Some("tool permission intent did not identify any resources".into()),
            ));
        }
        let structured_rules = self
            .applicable_rules_within(&full_request, scopes.plan_scoped, include_builtin_allows)
            .map_err(|error| {
                warn!(%error, "structured permission policy failed closed");
                deny(DECISION_SOURCE_RULE, Some(error.to_string()))
            })?;
        if self.configured().remote_policy_invalid {
            return Err(deny(
                DECISION_SOURCE_RULE,
                Some("remote permission policy is unavailable".into()),
            ));
        }
        if structured_rules
            .iter()
            .any(|policy| permission_rule_intersects_request(&policy.rule, &full_request))
        {
            return Err(deny(DECISION_SOURCE_RULE, None));
        }
        let full = self.request_coverage(&full_request, &structured_rules, include_builtin_allows);
        if self.remote_default_denies(tool, &full_request) {
            return Err(deny(DECISION_SOURCE_RULE, None));
        }
        if self.is_yolo() {
            return allowed(by_rule());
        }
        let all_resources_resolved = full
            .covered
            .iter()
            .zip(&full.prompt_required)
            .all(|(covered, must_prompt)| covered.is_some() || *must_prompt);
        let (t2, s2, force_prompt) = if all_resources_resolved {
            if !scopes.force_prompt && !full.must_prompt {
                return allowed(DECISION_SOURCE_RULE);
            }
            (tool.clone(), scopes.scopes.clone(), force_prompt)
        } else {
            if exact_plan_write && !force_prompt && !full.must_prompt {
                return allowed(DECISION_SOURCE_RULE);
            }
            // Silence is answered by the default effect, which is not a rule:
            // it says what an unmatched call means rather than matching one.
            match self.default_effect(tool) {
                DefaultEffect::Allow if !force_prompt && !full.must_prompt => {
                    return allowed(by_rule());
                }
                DefaultEffect::Deny if !force_prompt => {
                    return Err(deny(DECISION_SOURCE_RULE, None));
                }
                DefaultEffect::Allow | DefaultEffect::Deny | DefaultEffect::Prompt => {
                    (tool.clone(), scopes.scopes.clone(), force_prompt)
                }
            }
        };

        let mut request = make_request(t2.clone(), s2.clone(), force_prompt);
        if scopes.plan_scoped {
            contain_authority_to_the_plan(&mut request);
        }
        let structured_rules = self
            .applicable_rules_within(&request, scopes.plan_scoped, include_builtin_allows)
            .map_err(|error| {
                warn!(%error, "structured permission policy failed closed");
                deny(DECISION_SOURCE_RULE, Some(error.to_string()))
            })?;
        if structured_rules
            .iter()
            .any(|policy| permission_rule_intersects_request(&policy.rule, &request))
        {
            return Err(deny(DECISION_SOURCE_RULE, None));
        }
        let coverage = self.request_coverage(&request, &structured_rules, include_builtin_allows);
        if !scopes.force_prompt
            && coverage.covered.iter().all(Option::is_some)
            && !coverage.must_prompt
        {
            return allowed(DECISION_SOURCE_RULE);
        }
        update_presentation_coverage(&mut request.presentation, &coverage.covered);

        let Some(_) = user_response_rx else {
            warn!(tool = %tool, scope = %scope_display(), "no permission response channel");
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

        let (answer_tx, answer_rx) = flume::bounded(1);
        let (forcing_reason, uncovered, uncovered_count) = {
            let mut pending = self.pending();
            let current_rules = self
                .applicable_rules_within(&request, scopes.plan_scoped, include_builtin_allows)
                .map_err(|error| {
                    warn!(%error, "structured permission policy failed closed");
                    deny(DECISION_SOURCE_RULE, Some(error.to_string()))
                })?;
            if current_rules
                .iter()
                .any(|policy| permission_rule_intersects_request(&policy.rule, &request))
            {
                return Err(deny(DECISION_SOURCE_RULE, None));
            }
            let mut coverage =
                self.request_coverage(&request, &current_rules, include_builtin_allows);
            if !scopes.force_prompt
                && coverage.covered.iter().all(Option::is_some)
                && !coverage.must_prompt
            {
                return allowed(DECISION_SOURCE_RULE);
            }
            if scopes.force_prompt {
                coverage.prompt_required.fill(true);
                coverage.rule_ask.fill(true);
            }
            update_presentation_coverage(&mut request.presentation, &coverage.covered);
            let requests = pending.entry(self.id).or_default();
            if requests.contains_key(request_id) {
                warn!(request_id, "duplicate permission request id");
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            requests.insert(
                request_id.to_owned(),
                PendingPermission {
                    request: request.clone(),
                    sticky_ask: coverage.rule_ask,
                    project: canonical_project,
                    event_tx: event_tx.clone(),
                    sender: answer_tx,
                },
            );
            if event_tx
                .send(AgentEvent::PermissionRequest(Box::new(request.clone())))
                .is_err()
            {
                remove_pending(&mut pending, self.id, request_id);
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            (
                prompt_forcing_reason(
                    &request,
                    &coverage.covered,
                    unreviewed,
                    coverage.must_prompt,
                ),
                uncovered_resource_summary(&request, &coverage.covered),
                coverage
                    .covered
                    .iter()
                    .filter(|covered| covered.is_none())
                    .count(),
            )
        };
        // Emitted outside the pending lock: a log write must never serialize
        // another agent's permission bookkeeping behind file IO.
        let (subject_owner, subject_contract) = subject_kind_and_contract(&request.subject);
        info!(
            target: PERMISSION_LOG_TARGET,
            event = "permission_prompt",
            request_id,
            tool = %request.tool,
            executor = ?request.executor,
            risk = ?request.risk,
            subject_owner,
            subject_contract,
            resource_count = request.resources.len(),
            uncovered_count,
            forcing_reason,
            uncovered = %uncovered,
            offered_options = %request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            "permission prompt raised"
        );
        let waiting_since = Instant::now();
        let response = cancel.race(answer_rx.recv_async()).await;
        self.remove_pending(request_id);

        let decision = match response {
            Ok(Ok(decision)) => Some(decision),
            Ok(Err(_)) => {
                warn!(tool = %tool, scope = %scope_display(), "permission channel closed");
                None
            }
            Err(_) => None,
        };
        // Paired with `permission_prompt` by `request_id`: the two together give
        // the prompt rate, what authority the answer bought, and the wait cost.
        let (answer, option_id, lifetime, answer_source) = match &decision {
            Some(PendingDecision::Explicit(explicit)) => {
                let (answer, option_id, lifetime) = answer_log_fields(explicit);
                (answer, option_id, lifetime, explicit.decision_source())
            }
            Some(PendingDecision::MatchedRule) => {
                ("matched_rule", Cow::Borrowed(""), "", DECISION_SOURCE_RULE)
            }
            None => (
                "abandoned",
                Cow::Borrowed(""),
                "",
                DECISION_SOURCE_USER_ABORT,
            ),
        };
        info!(
            target: PERMISSION_LOG_TARGET,
            event = "permission_decision",
            request_id,
            tool = %request.tool,
            answer,
            option_id = %option_id,
            lifetime,
            source = answer_source,
            waited_ms = waiting_since.elapsed().as_millis() as u64,
            "permission prompt answered"
        );
        let Some(decision) = decision else {
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

        let allow = match &decision {
            PendingDecision::Explicit(answer) => answer.is_allow(),
            PendingDecision::MatchedRule => true,
        };
        if allow {
            let current_rules = self
                .applicable_rules_within(&request, scopes.plan_scoped, include_builtin_allows)
                .map_err(|error| {
                    warn!(%error, "structured permission policy failed closed");
                    deny(DECISION_SOURCE_RULE, Some(error.to_string()))
                })?;
            if current_rules
                .iter()
                .any(|policy| permission_rule_intersects_request(&policy.rule, &request))
            {
                return Err(deny(DECISION_SOURCE_RULE, None));
            }
            if matches!(decision, PendingDecision::MatchedRule) {
                let coverage =
                    self.request_coverage(&request, &current_rules, include_builtin_allows);
                if scopes.force_prompt
                    || coverage.must_prompt
                    || !coverage.covered.iter().all(Option::is_some)
                {
                    return Err(deny(DECISION_SOURCE_RULE, None));
                }
            }
        }
        let source = match &decision {
            PendingDecision::Explicit(answer) => answer.decision_source(),
            PendingDecision::MatchedRule => DECISION_SOURCE_RULE,
        };
        if allow {
            allowed(source)
        } else {
            let guidance = match decision {
                PendingDecision::Explicit(answer) => answer.guidance().map(String::from),
                PendingDecision::MatchedRule => None,
            };
            Err(deny(source, guidance))
        }
    }
}

fn subject_kind_and_contract(subject: &PermissionSubject) -> (&str, &str) {
    match subject {
        PermissionSubject::Native { owner, contract } => (owner, contract),
        PermissionSubject::Lua {
            plugin, contract, ..
        } => (plugin, contract),
        PermissionSubject::Mcp {
            server, contract, ..
        } => (server, contract),
        PermissionSubject::RemoteWorkcell {
            identity, contract, ..
        } => (identity.authority.server_id(), contract),
        PermissionSubject::RemoteNative {
            owner, contract, ..
        } => (owner, contract),
        PermissionSubject::UnknownLegacy { identity } => (identity, ""),
    }
}

/// Answer kind, chosen option, and granted lifetime as separate fields, so
/// prompt analysis can group by lifetime without parsing `encode()`. Denials
/// carry no lifetime because a `deny_always_*` writes its rule elsewhere.
fn answer_log_fields(answer: &PermissionAnswer) -> (&'static str, Cow<'_, str>, &'static str) {
    let none = Cow::Borrowed("");
    match answer {
        PermissionAnswer::AllowOnce => ("allow", none, lifetime_name(&PermissionLifetime::Once)),
        PermissionAnswer::AllowSession => (
            "allow_session",
            none,
            lifetime_name(&PermissionLifetime::Conversation),
        ),
        PermissionAnswer::AllowAlwaysLocal => (
            "allow_always_local",
            none,
            lifetime_name(&PermissionLifetime::Project),
        ),
        PermissionAnswer::AllowAlwaysGlobal => (
            "allow_always_global",
            none,
            lifetime_name(&PermissionLifetime::Global),
        ),
        PermissionAnswer::AllowOption {
            option_id,
            lifetime,
        } => (
            "allow_option",
            Cow::Borrowed(option_id.as_str()),
            lifetime_name(lifetime),
        ),
        // The rungs themselves are not named: what matters for grouping is how
        // much of the request an answer chose to remember.
        PermissionAnswer::AllowComposed { rows, lifetime } => (
            "allow_composed",
            Cow::Owned(format!(
                "{}/{} rows",
                rows.iter().filter(|row| row.is_some()).count(),
                rows.len()
            )),
            lifetime_name(lifetime),
        ),
        PermissionAnswer::Deny => ("deny", none, ""),
        PermissionAnswer::DenyWithGuidance(_) => ("deny_guidance", none, ""),
        PermissionAnswer::DenyAlwaysLocal => ("deny_always_local", none, ""),
        PermissionAnswer::DenyAlwaysGlobal => ("deny_always_global", none, ""),
    }
}

/// Withdraws the lifetimes that would outlive the plan being built.
///
/// `commit_structured_decision` validates an answer against the lifetimes the
/// request carried, so withdrawing them here is what refuses a project or
/// global answer from any client, not just from a prompt that hid the keys.
/// Denials keep theirs: a plan may not widen authority, but narrowing it is
/// always the user's to make.
fn contain_authority_to_the_plan(request: &mut PermissionRequest) {
    for option in &mut request.options {
        if option.rule.effect != StructuredPermissionEffect::Allow {
            continue;
        }
        option.allowed_lifetimes.retain(|lifetime| {
            matches!(
                lifetime,
                PermissionLifetime::Once | PermissionLifetime::Conversation
            )
        });
    }
}

/// The first reason that applies, so a log line names one cause rather than a
/// set. `ask_rule` covers both a builtin ask family and a configured ask;
/// telling them apart would mean widening what `request_coverage` returns.
fn prompt_forcing_reason(
    request: &PermissionRequest,
    coverage: &[Option<ResourceCoverage>],
    forced: bool,
    ask_rule: bool,
) -> &'static str {
    let uncovered = || {
        request
            .resources
            .iter()
            .enumerate()
            .filter(move |(index, _)| coverage.get(*index).is_none_or(Option::is_none))
    };
    if forced {
        PROMPT_REASON_FORCED
    } else if uncovered().any(|(_, resource)| resource.protected) {
        PROMPT_REASON_PROTECTED
    } else if uncovered().any(|(_, resource)| resource.requires_prompt) {
        PROMPT_REASON_REQUIRES_PROMPT
    } else if ask_rule {
        PROMPT_REASON_ASK_RULE
    } else {
        PROMPT_REASON_UNCOVERED
    }
}

/// Only the resources that actually forced the prompt, capped, so a chain of
/// twenty already-approved commands does not bury the one that is new.
fn uncovered_resource_summary(
    request: &PermissionRequest,
    coverage: &[Option<ResourceCoverage>],
) -> String {
    let mut summary = String::new();
    let uncovered = request
        .resources
        .iter()
        .enumerate()
        .filter(|(index, _)| coverage.get(*index).is_none_or(Option::is_none));
    for (count, (_, resource)) in uncovered.enumerate() {
        if count == PROMPT_LOG_MAX_RESOURCES {
            let _ = write!(summary, ", ...");
            break;
        }
        if count > 0 {
            summary.push_str(", ");
        }
        let value: String = resource
            .value
            .chars()
            .take(PROMPT_LOG_MAX_VALUE_CHARS)
            .collect();
        let _ = write!(summary, "{:?}:{value}", resource.kind);
    }
    summary
}

fn matches_rule(rule_key: &ToolKey, actual: &ToolKey) -> bool {
    match (rule_key, actual) {
        (ToolKey::Wildcard, _) => true,
        (ToolKey::Native(a), ToolKey::Native(b)) => a == b,
        (ToolKey::McpServer { server: rs }, ToolKey::McpServer { server: as_ }) => rs == as_,
        (ToolKey::McpServer { server: rs }, ToolKey::McpTool { server: as_, .. }) => rs == as_,
        (
            ToolKey::McpTool {
                server: rs,
                tool: rt,
            },
            ToolKey::McpTool {
                server: as_,
                tool: at,
            },
        ) => rs == as_ && rt == at,
        _ => false,
    }
}

fn is_shell_tool(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(name) if matches!(name.as_ref(), "bash" | "shell"))
}

fn command_rule_tool_matches(rule: &ToolKey, actual: &ToolKey) -> bool {
    matches!(rule, ToolKey::Wildcard)
        || is_shell_tool(rule) && is_shell_tool(actual)
        || matches_rule(rule, actual)
}

fn is_bound_shell_request(request: &PermissionRequest) -> bool {
    if !is_shell_tool(&request.tool)
        || request.executor != PermissionExecutorKind::Native
        || request.resources.is_empty()
        || !request
            .resources
            .iter()
            .all(|resource| resource.kind == PermissionResourceKind::Command)
    {
        return false;
    }
    match &request.subject {
        PermissionSubject::Native { owner, contract } => {
            owner == "workcell" && contract == "shell.execution.v1"
        }
        PermissionSubject::Lua { .. }
        | PermissionSubject::Mcp { .. }
        | PermissionSubject::RemoteWorkcell { .. }
        | PermissionSubject::RemoteNative { .. }
        | PermissionSubject::UnknownLegacy { .. } => false,
    }
}

pub fn shell_permission_scope(command: &str, workdir: &Path) -> String {
    let workdir = workdir.to_string_lossy();
    format!(
        "{command}{BASH_WORKDIR_SCOPE_MARKER}{}]={workdir}{BASH_WORKDIR_FRAME_MARKER}{}]",
        workdir.len(),
        workdir.len()
    )
}

fn bash_scope_parts(scope: &str) -> Option<(&str, &str)> {
    let (payload, frame) = scope.rsplit_once(BASH_WORKDIR_FRAME_MARKER)?;
    let workdir_length = frame.strip_suffix(']')?.parse::<usize>().ok()?;
    let workdir_start = payload.len().checked_sub(workdir_length)?;
    let command_with_metadata = payload.get(..workdir_start)?;
    let workdir = payload.get(workdir_start..)?;
    let metadata = format!("{BASH_WORKDIR_SCOPE_MARKER}{workdir_length}]=");
    Some((command_with_metadata.strip_suffix(&metadata)?, workdir))
}

/// Glob matcher for permission scopes. The boundary suffixes (`/**`, `" *"`)
/// must be tried before the bare `*`, otherwise a plain prefix would swallow
/// them. `" *"` is the bash form `<command> *`: it has to match the bare
/// command too (`pwd *` covers `pwd` and `pwd -L`, but not `pwdx`).
///
/// For the `/**` path pattern, `Path::starts_with` is used to compare
/// components rather than characters, which handles both `/` and `\`
/// transparently on all platforms.
pub fn scope_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        // Normalize both sides the same way: absolutize, then resolve symlinks
        // in existing leading components before appending the lexical tail.
        // Absolutizing first keeps a relative rule like `dist/**` matching
        // before the dir exists, since `incremental_canonicalize` leaves a
        // relative path relative when the leading component is missing.
        let norm = |p: &str| {
            let abs = std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p));
            caudra_storage::paths::incremental_canonicalize(&abs)
                .unwrap_or_else(|| caudra_storage::paths::normalize_path(&abs))
        };
        let norm_prefix = norm(prefix);
        let norm_value = norm(value);
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return value == prefix || value.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

/// Lexical normalization for scope paths. Resolves `..` and `.` without
/// hitting the filesystem and without producing `\\?\` prefixes on Windows.
/// Use this for display, logging, and scope matching.
///
/// For symlink-aware security checks, use [`physical_boundary_check`].
pub fn normalize_scope_path(path: &str) -> String {
    let resolved = crate::tools::resolve_path(path).unwrap_or_else(|_| path.to_string());
    caudra_storage::paths::normalize_path(Path::new(&resolved))
        .to_string_lossy()
        .into_owned()
}

/// Check whether `child` is physically inside `parent`, following symlinks.
///
/// Uses incremental left-to-right canonicalization: each component is
/// resolved through the filesystem (including symlinks) *before* any
/// subsequent `..` component can act on it. This prevents symlink-based
/// boundary escapes where a symlink followed by `..` resolves to a
/// location outside the parent.
///
/// Returns `true` only when the resolved filesystem location of `child`
/// is under `parent`. Returns `None` if the parent itself cannot be resolved.
pub fn physical_boundary_check(parent: &Path, child: &Path) -> Option<bool> {
    let parent_canon = caudra_storage::paths::incremental_canonicalize(parent)?;
    let child_canon = caudra_storage::paths::incremental_canonicalize(child)
        .unwrap_or_else(|| child.to_path_buf());
    Some(child_canon.starts_with(&parent_canon))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::tools::PermissionIntent;
    use caudra_storage::sessions::SessionDatabase;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, ResourceId,
        ResourceRevision, SourceTrustAnchor, WorkspacePath,
    };
    use test_case::test_case;

    const PERMISSION_RULES_STATE_KEY: &str = "permission.rules";
    const SHELL_WORKDIR: &str = "/tmp";
    const LEGACY_REQUEST_ID: &str = "legacy-request";

    fn make_config(rules: Vec<PermissionRule>) -> PermissionsConfig {
        PermissionsConfig {
            rules,
            ..Default::default()
        }
    }

    fn allow_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some(scope.into()),
            effect: Effect::Allow,
        }
    }

    fn deny_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some(scope.into()),
            effect: Effect::Deny,
        }
    }

    fn remote_permission_asset(
        revision: &str,
        digest: &str,
        allow_scope: &str,
    ) -> crate::remote_project_context::RemotePermissionAsset {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("https://workcell.example").unwrap(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        crate::remote_project_context::RemotePermissionAsset {
            source: crate::remote_project_context::RemoteAssetIdentity {
                principal: AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
                project: ProjectIdentity::new(
                    authority.clone(),
                    ProjectKey::new("project").unwrap(),
                ),
                authority,
                path: WorkspacePath::new(".caudra/permissions.toml").unwrap(),
                resource_id: ResourceId::new("permissions").unwrap(),
                revision: ResourceRevision::new(revision).unwrap(),
            },
            digest: digest.into(),
            declarations: crate::remote_project_context::RemotePermissionDeclarations {
                restrictive_rules: vec![deny_rule("remote-denied")],
                allow_rules: vec![allow_rule(allow_scope)],
                ..Default::default()
            },
        }
    }

    /// What the one evaluator says about each resource, over the configured
    /// policy compiled against the request.
    fn decisions(
        manager: &PermissionManager,
        request: &PermissionRequest,
    ) -> Vec<StructuredPermissionDecision> {
        let rules = manager.configured_structured_rules(request, true);
        request
            .resources
            .iter()
            .map(|resource| permission_rules_resource_standing(&rules, request, resource).decision)
            .collect()
    }

    /// Coverage the way `enforce` computes it: the configured, builtin, and
    /// plugin policy compiled against the request, plus whatever stored rules
    /// the case is about.
    fn coverage_with(
        manager: &PermissionManager,
        request: &PermissionRequest,
        builtin_allows: bool,
        stored: &[PolicyRule],
    ) -> RequestCoverage {
        let mut rules = stored.to_vec();
        rules.extend(manager.configured_structured_rules(request, builtin_allows));
        manager.request_coverage(request, &rules, builtin_allows)
    }

    /// Which resources carry authority, for cases about coverage rather than
    /// about the authority that granted it.
    fn covered_flags(coverage: &RequestCoverage) -> Vec<bool> {
        coverage.covered.iter().map(Option::is_some).collect()
    }

    /// A stored grant, which is what a persisted record compiles to.
    fn stored_policy(rule: StructuredPermissionRule) -> PolicyRule {
        PolicyRule {
            origin: RuleOrigin::Project,
            rule,
        }
    }

    /// A call the rules settle on their own: every resource carries authority
    /// and nothing withholds it, so `enforce` returns without prompting.
    fn allows_without_prompt(manager: &PermissionManager, request: &PermissionRequest) -> bool {
        let coverage = coverage_with(manager, request, true, &[]);
        coverage.covered.iter().all(Option::is_some) && !coverage.must_prompt
    }

    /// A restrictive rule reaching the request, which is the one answer that
    /// outranks every grant, yolo, and the default effect.
    fn denied_by_rule(manager: &PermissionManager, request: &PermissionRequest) -> bool {
        manager
            .applicable_rules_within(request, false, true)
            .unwrap()
            .iter()
            .any(|policy| permission_rule_intersects_request(&policy.rule, request))
    }

    /// The default effect answers what no rule spoke to, so a deny default only
    /// blocks the resources the rules left uncovered.
    fn denied_by_default(manager: &PermissionManager, request: &PermissionRequest) -> bool {
        matches!(manager.default_effect(&request.tool), DefaultEffect::Deny)
            && !coverage_with(manager, request, true, &[])
                .covered
                .iter()
                .all(Option::is_some)
    }

    fn allowed_by_default(manager: &PermissionManager, request: &PermissionRequest) -> bool {
        matches!(manager.default_effect(&request.tool), DefaultEffect::Allow)
            && !coverage_with(manager, request, true, &[]).must_prompt
    }

    fn legacy_request(
        manager: &PermissionManager,
        tool: ToolKey,
        scopes: &[&str],
    ) -> PermissionRequest {
        PermissionRequest::from_legacy(
            LEGACY_REQUEST_ID.into(),
            tool,
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            serde_json::Value::Null,
            &manager.project_cwd(),
            false,
        )
    }

    fn shell_policy_rule(scope: &str, effect: Effect) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some(scope.into()),
            effect,
        }
    }

    fn shell_intent(commands: &[&str]) -> crate::tools::PermissionIntent {
        let workdir = SHELL_WORKDIR;
        crate::tools::PermissionIntent::new(
            crate::tools::PermissionScopes {
                scopes: commands.iter().map(|command| (*command).into()).collect(),
                force_prompt: false,
                plan_scoped: false,
            },
            commands
                .iter()
                .map(|command| PermissionResource {
                    kind: PermissionResourceKind::Command,
                    value: (*command).into(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::from([("workdir".into(), workdir.into())]),
                })
                .collect(),
            PermissionRisk::High,
        )
        .with_authority(PermissionAuthorityProfile::Shell)
    }

    fn shell_request(commands: &[&str], subject: PermissionSubject) -> PermissionRequest {
        let workdir = SHELL_WORKDIR;
        let intent = shell_intent(commands);
        PermissionRequest::from_intent_with_identity(
            "shell-request".into(),
            ToolKey::native("shell"),
            &intent,
            serde_json::json!({"command": commands.join(" && "), "workdir": workdir}),
            Path::new(workdir),
            subject,
            PermissionExecutorKind::Native,
        )
    }

    /// Runs the real evaluator over a shell call with no response channel, so a
    /// call it cannot settle on the rules alone reports the refusal instead of
    /// waiting on a prompt nobody will answer.
    async fn enforce_shell_without_prompt(
        manager: &PermissionManager,
        commands: &[&str],
        force_prompt: bool,
    ) -> Result<(), PermissionError> {
        let mut intent = shell_intent(commands);
        intent.scopes.force_prompt = force_prompt;
        let (event_tx, _event_rx) = flume::unbounded();
        manager
            .enforce_with_intent(
                &ToolKey::native("shell"),
                &intent,
                &serde_json::json!({"command": commands.join(" && "), "workdir": SHELL_WORKDIR}),
                &crate::EventSender::new(event_tx, 0),
                None,
                "shell-request",
                &crate::CancelToken::none(),
                None,
                Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                true,
            )
            .await
    }

    fn workcell_shell_subject() -> PermissionSubject {
        PermissionSubject::Native {
            owner: "workcell".into(),
            contract: "shell.execution.v1".into(),
        }
    }

    fn mgr_with(config: PermissionsConfig, cwd: PathBuf) -> PermissionManager {
        PermissionManager::new_nonpersistent(config, cwd, Arc::default())
    }

    fn default_mgr() -> PermissionManager {
        mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"))
    }

    #[test]
    fn shell_config_uses_specificity_and_ask_wins_ties() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule("*", Effect::Ask),
                shell_policy_rule("git status *", Effect::Allow),
            ]),
            PathBuf::from("/tmp"),
        );
        let status = shell_request(&["git status --short"], workcell_shell_subject());
        let commit = shell_request(&["git commit -m message"], workcell_shell_subject());

        assert_eq!(
            decisions(&manager, &status),
            vec![StructuredPermissionDecision::Allow]
        );
        assert_eq!(
            decisions(&manager, &commit),
            vec![StructuredPermissionDecision::Ask]
        );
        assert!(allows_without_prompt(&manager, &status));
        assert!(coverage_with(&manager, &commit, true, &[]).must_prompt);

        let tied = mgr_with(
            make_config(vec![
                shell_policy_rule("git status *", Effect::Allow),
                shell_policy_rule("git status *", Effect::Ask),
            ]),
            PathBuf::from("/tmp"),
        );
        assert_eq!(
            decisions(&tied, &status),
            vec![StructuredPermissionDecision::Ask]
        );
    }

    const CONFINED_COMMAND: &str = "git status --short";

    fn mark_confined(request: &mut PermissionRequest) {
        for resource in &mut request.resources {
            resource.attributes.insert(
                CONFINED_READ_ATTRIBUTE.into(),
                CONFINED_READ_VALUE.to_owned(),
            );
        }
    }

    /// The classifier only ever kept plan mode from refusing such a line; it
    /// still cost a prompt in both modes. The builtin rule is what makes the
    /// resource covered, so nothing has to be asked.
    #[test_case(true => vec![true] ; "a confined read needs no prompt")]
    #[test_case(false => vec![false] ; "the same line unmarked still does")]
    fn a_confined_read_is_covered_by_the_builtin_rule(confined: bool) -> Vec<bool> {
        let manager = default_mgr();
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        if confined {
            mark_confined(&mut request);
        }

        covered_flags(&coverage_with(
            &manager,
            &request,
            false,
            &builtin_structured_rules(),
        ))
    }

    /// A prompt has to name the authority covering a row, and this rule's
    /// selector is `Any`. Read off the selector alone it would tell the user
    /// every command is allowed, when the rule reaches only what the shell tool
    /// marked. Nothing showed the string before, because a line whose every row
    /// is confined raises no prompt to read it off.
    #[test]
    fn a_confined_read_names_the_reason_rather_than_its_selector() {
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        mark_confined(&mut request);

        assert_eq!(
            coverage_with(&default_mgr(), &request, false, &builtin_structured_rules())
                .covered
                .swap_remove(0),
            Some(ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: CONFINED_READ_AUTHORITY.into(),
            })
        );
    }

    /// Coverage is only worth anything if the manager actually consults the
    /// builtin rule when it collects the applicable set, which no test that
    /// hands the rule in directly can show.
    #[test_case(true => true ; "a confined read is enforced without a responder")]
    #[test_case(false => false ; "the same line unmarked cannot be")]
    fn the_manager_applies_the_builtin_rule_it_owns(confined: bool) -> bool {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().to_path_buf();
            let manager = mgr_with(PermissionsConfig::default(), project.clone());
            let mut intent = shell_intent(&[CONFINED_COMMAND]);
            if confined {
                for resource in &mut intent.resources {
                    resource.attributes.insert(
                        CONFINED_READ_ATTRIBUTE.into(),
                        CONFINED_READ_VALUE.to_owned(),
                    );
                }
            }
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            manager
                .enforce_with_intent(
                    &ToolKey::native("shell"),
                    &intent,
                    &serde_json::json!({}),
                    &event_tx,
                    None,
                    "confined-read",
                    &crate::CancelToken::none(),
                    None,
                    Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                    true,
                )
                .await
                .is_ok()
        })
    }

    /// The attribute is the whole justification, so it only justifies the tool
    /// that earns it. A plugin presenting the same command must not inherit it.
    #[test]
    fn a_confined_read_does_not_cross_to_another_subject() {
        let manager = default_mgr();
        let mut request = shell_request(
            &[CONFINED_COMMAND],
            PermissionSubject::Lua {
                plugin: "untrusted".into(),
                tool: "shell".into(),
                contract: "shell.execution.v1".into(),
            },
        );
        mark_confined(&mut request);

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    #[test]
    fn a_local_confined_read_never_covers_a_remote_resource() {
        let manager = default_mgr();
        let asset = remote_permission_asset(
            "revision",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "unused",
        );
        let identity = RemotePermissionIdentity {
            authority: asset.source.authority.clone(),
            principal: asset.source.principal.clone(),
            project: asset.source.project.clone(),
        };
        let intent = PermissionIntent::new(
            crate::tools::PermissionScopes::single("remote read".into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::RemoteFile {
                    identity: identity.clone(),
                },
                value: "root\u{1f}file".into(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([(
                    CONFINED_READ_ATTRIBUTE.into(),
                    CONFINED_READ_VALUE.into(),
                )]),
            }],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::RemoteResource);
        let request = PermissionRequest::from_intent_with_identity(
            "remote-confined".into(),
            ToolKey::native("file_read"),
            &intent,
            serde_json::json!({}),
            Path::new("/tmp"),
            PermissionSubject::RemoteNative {
                identity,
                owner: "caudra".into(),
                contract: "view-image/v1".into(),
            },
            PermissionExecutorKind::Native,
        );

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    /// Belt and braces. The shell tool never marks an opaque line, and an opaque
    /// line is protected, which the rule refuses to cover on its own.
    #[test]
    fn a_protected_command_is_not_covered_even_when_marked() {
        let manager = default_mgr();
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        mark_confined(&mut request);
        for resource in &mut request.resources {
            resource.protected = true;
        }

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    #[test]
    fn shell_config_allow_requires_the_workcell_contract() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("git status *", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let spoofed = shell_request(
            &["git status --short"],
            PermissionSubject::Lua {
                plugin: "untrusted".into(),
                tool: "shell".into(),
                contract: "shell.execution.v1".into(),
            },
        );

        assert_eq!(
            decisions(&manager, &spoofed),
            vec![StructuredPermissionDecision::NoMatch]
        );
        let workcell = shell_request(&["git status --short"], workcell_shell_subject());
        assert_eq!(
            decisions(&manager, &workcell),
            vec![StructuredPermissionDecision::Allow]
        );
    }

    const COVERAGE_COMMAND: &str = "git status --short";
    const COVERAGE_PATTERN: &str = "git status *";
    const ECHO_COMMAND: &str = "echo hi";
    const BUILTIN_ECHO_PATTERN: &str = "echo *";
    const THIS_COMMAND_AUTHORITY: &str = "this command";
    const BROAD_GIT_PATTERN: &str = "git *";

    fn coverage_of(
        manager: &PermissionManager,
        command: &str,
        stored: &[PolicyRule],
    ) -> Option<ResourceCoverage> {
        let request = shell_request(&[command], workcell_shell_subject());
        coverage_with(manager, &request, true, stored)
            .covered
            .swap_remove(0)
    }

    fn conversation_grant(command: &str) -> PolicyRule {
        let request = shell_request(&[command], workcell_shell_subject());
        PolicyRule {
            origin: RuleOrigin::Conversation,
            rule: request
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap(),
        }
    }

    /// A covered resource has to say which authority covers it: a prompt that
    /// only says "already allowed" cannot be acted on, and the lifetime alone
    /// would report configured policy as something the user granted.
    #[test]
    fn coverage_names_the_origin_and_the_authority_that_carries_it() {
        let configured = mgr_with(
            make_config(vec![shell_policy_rule(COVERAGE_PATTERN, Effect::Allow)]),
            PathBuf::from(SHELL_WORKDIR),
        );

        assert_eq!(
            coverage_of(&configured, COVERAGE_COMMAND, &[]),
            Some(ResourceCoverage {
                origin: RuleOrigin::Config,
                authority: COVERAGE_PATTERN.into(),
            })
        );
        assert_eq!(
            coverage_of(&default_mgr(), ECHO_COMMAND, &[]),
            Some(ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: BUILTIN_ECHO_PATTERN.into(),
            })
        );
        assert_eq!(
            coverage_of(
                &default_mgr(),
                COVERAGE_COMMAND,
                &[conversation_grant(COVERAGE_COMMAND)]
            ),
            Some(ResourceCoverage {
                origin: RuleOrigin::Conversation,
                authority: THIS_COMMAND_AUTHORITY.into(),
            })
        );
    }

    /// A deny leaves nothing covered even when an allow reaches the resource,
    /// so the prompt cannot claim an authority the call does not have.
    #[test]
    fn a_denied_resource_carries_no_coverage() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule(BROAD_GIT_PATTERN, Effect::Allow),
                shell_policy_rule(COVERAGE_PATTERN, Effect::Deny),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );

        assert_eq!(coverage_of(&manager, COVERAGE_COMMAND, &[]), None);
    }

    /// Configured policy and a grant of equal reach both cover the command. The
    /// grant is the one the user made, so it is the one the prompt credits.
    #[test]
    fn a_grant_outranks_configured_policy_of_equal_reach() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule(COVERAGE_COMMAND, Effect::Allow)]),
            PathBuf::from(SHELL_WORKDIR),
        );

        assert_eq!(
            coverage_of(
                &manager,
                COVERAGE_COMMAND,
                &[conversation_grant(COVERAGE_COMMAND)]
            )
            .map(|coverage| coverage.origin),
            Some(RuleOrigin::Conversation)
        );
    }

    /// An ask withholds authority without erasing it, so the covering allow is
    /// still reported and a later grant can still sweep the prompt.
    #[test]
    fn an_ask_still_reports_the_allow_that_covers_the_resource() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule(BROAD_GIT_PATTERN, Effect::Allow),
                shell_policy_rule(COVERAGE_PATTERN, Effect::Ask),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&[COVERAGE_COMMAND], workcell_shell_subject());

        let coverage = coverage_with(&manager, &request, true, &[]);

        assert!(coverage.must_prompt);
        assert_eq!(
            coverage.covered[0],
            Some(ResourceCoverage {
                origin: RuleOrigin::Config,
                authority: BROAD_GIT_PATTERN.into(),
            })
        );
    }

    /// `cmd *` is a command pattern, not a text prefix: it has always covered
    /// the bare invocation as well as the one with arguments, and it stops at a
    /// token boundary so it cannot reach a longer executable name.
    #[test]
    fn a_configured_command_pattern_covers_the_bare_invocation_only_to_its_token_boundary() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("pwd *", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let covered = |command: &str| {
            let request = shell_request(&[command], workcell_shell_subject());
            covered_flags(&coverage_with(&manager, &request, false, &[]))
        };

        assert_eq!(covered("pwd"), [true]);
        assert_eq!(covered("pwd -L"), [true]);
        assert_eq!(covered("pwdx 1"), [false]);
    }

    /// A deny has to reach the impostor the allow refuses to reach, otherwise
    /// claiming the shell contract would buy exemption from restrictive policy.
    #[test]
    fn shell_config_denies_reach_a_spoofed_contract() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("git status *", Effect::Deny)]),
            PathBuf::from("/tmp"),
        );
        let spoofed = shell_request(
            &["git status --short"],
            PermissionSubject::Lua {
                plugin: "untrusted".into(),
                tool: "shell".into(),
                contract: "shell.execution.v1".into(),
            },
        );

        assert_eq!(
            decisions(&manager, &spoofed),
            vec![StructuredPermissionDecision::Deny]
        );
    }

    #[test]
    fn command_denies_match_static_quoted_tokens() {
        let manager = mgr_with(
            PermissionsConfig {
                yolo: true,
                rules: vec![shell_policy_rule("git commit *", Effect::Deny)],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        let quoted = shell_request(&[r#"git "commit" -m message"#], workcell_shell_subject());
        assert_eq!(
            decisions(&manager, &quoted),
            vec![StructuredPermissionDecision::Deny]
        );

        // A protected command the deny does not name is no longer refused for
        // the mere existence of a deny against the tool. It is left uncovered,
        // so it prompts: no configured allow can reach a protected resource.
        let mut opaque = shell_request(&["eval command"], workcell_shell_subject());
        opaque.resources[0].protected = true;
        assert_eq!(
            decisions(&manager, &opaque),
            vec![StructuredPermissionDecision::NoMatch]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &opaque, true, &[])),
            [false]
        );
    }

    /// A configured allow is written against a command's reviewed text, which a
    /// protected command outruns: its redirects and expansions say more than
    /// the scope does.
    #[test]
    fn a_configured_allow_never_reaches_a_protected_command() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("*", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let mut request = shell_request(&["git status --short"], workcell_shell_subject());
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [true]
        );

        request.resources[0].protected = true;
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [false]
        );
    }

    #[test]
    fn normalized_executable_names_apply_only_to_restrictive_shell_policy() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule("git status *", Effect::Allow),
                shell_policy_rule("rm *", Effect::Deny),
                shell_policy_rule("curl *", Effect::Ask),
            ]),
            PathBuf::from("/tmp"),
        );
        let request = |source: &str, normalized: &str| {
            let mut request = shell_request(&[source], workcell_shell_subject());
            request.resources[0]
                .attributes
                .insert(NORMALIZED_COMMAND_ATTRIBUTE.into(), normalized.into());
            request
        };

        assert_eq!(
            decisions(
                &manager,
                &request("/tmp/git status --short", "git status --short")
            ),
            vec![StructuredPermissionDecision::NoMatch]
        );
        assert_eq!(
            decisions(&manager, &request("/bin/rm -rf build", "rm -rf build")),
            vec![StructuredPermissionDecision::Deny]
        );
        assert_eq!(
            decisions(
                &manager,
                &request("/usr/bin/curl example.com", "curl example.com")
            ),
            vec![StructuredPermissionDecision::Ask]
        );

        let builtin_manager = mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"));
        let request = request("/bin/rm -rf build", "rm -rf build");
        let coverage = coverage_with(&builtin_manager, &request, true, &[]);
        assert!(coverage.must_prompt);
        assert_eq!(coverage.prompt_required, vec![true]);
    }

    const GIT_HEAD: &str = ".git/HEAD";
    const GIT_CONFIG: &str = ".git/config";

    fn project_read_request(cwd: &Path, relative: &str) -> PermissionRequest {
        let path = cwd.join(relative);
        let intent = crate::tools::PermissionIntent::new(
            crate::tools::PermissionScopes::single(path.to_string_lossy().into_owned()),
            vec![filesystem_permission_resource(
                PermissionResourceKind::File,
                &path,
                PermissionResourceAccess::Read,
                cwd,
            )],
            PermissionRisk::Low,
        );
        PermissionRequest::from_intent(
            "read".into(),
            ToolKey::native("file_read"),
            &intent,
            serde_json::json!({"filePath": path.to_string_lossy()}),
            cwd,
        )
    }

    /// The builtin project-read allow already matched `.git/HEAD`; the resource
    /// flag vetoed it, so every session re-prompted for the repository reading
    /// its own state. `config` holds remote credentials and must keep prompting.
    #[test_case(GIT_HEAD => true ; "inert_git_metadata_needs_no_prompt")]
    #[test_case(GIT_CONFIG => false ; "git_config_still_prompts")]
    fn project_reads_of_git_metadata(relative: &str) -> bool {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_path_buf();
        let manager = mgr_with(PermissionsConfig::default(), cwd.clone());
        let request = project_read_request(&cwd, relative);

        let coverage = coverage_with(&manager, &request, true, &[]);
        coverage.covered.iter().all(Option::is_some) && !coverage.must_prompt
    }

    #[test]
    fn explicit_allow_overrides_the_builtin_ask_fallback() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("rm *", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let request = shell_request(&["rm build.log"], workcell_shell_subject());
        let coverage = coverage_with(&manager, &request, true, &[]);

        assert_eq!(covered_flags(&coverage), vec![true]);
        assert!(!coverage.must_prompt);
    }

    #[test]
    fn configured_patterns_cover_repetitive_shell_chains_per_command() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule("git diff *", Effect::Allow),
                shell_policy_rule("git status *", Effect::Allow),
                shell_policy_rule("printf *", Effect::Allow),
                shell_policy_rule("pgrep *", Effect::Allow),
                shell_policy_rule("true", Effect::Allow),
            ]),
            PathBuf::from("/tmp"),
        );
        for commands in [
            vec![
                "git diff --check -- site/docs/content/cli/_index.md",
                "git diff -- site/docs/content/cli/_index.md",
                r#"printf '\n-- status --\n'"#,
                "git status --short",
            ],
            vec![
                "pgrep -af '(^|/)(caudra|maki)( |$)'",
                "true",
                "git status --short",
                "git diff --check",
            ],
        ] {
            let request = shell_request(&commands, workcell_shell_subject());
            let coverage = coverage_with(&manager, &request, true, &[]);
            assert!(coverage.covered.iter().all(Option::is_some));
            assert!(!coverage.must_prompt);
        }
    }

    #[test]
    fn project_shell_allows_require_exact_config_trust() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let config = PermissionsConfig {
            project_allow_rules: vec![shell_policy_rule("git diff *", Effect::Allow)],
            ..Default::default()
        };
        let manager = PermissionManager::new_persistent_in(
            config,
            project.clone(),
            Arc::default(),
            state_dir.clone(),
        );
        let request = shell_request(&["git diff --check"], workcell_shell_subject());

        assert!(manager.needs_project_permission_config_trust());
        assert_eq!(
            decisions(&manager, &request),
            vec![StructuredPermissionDecision::NoMatch]
        );
        manager.trust_project_permission_config().unwrap();
        assert!(!manager.needs_project_permission_config_trust());
        assert!(manager.project_permission_config_trusted());
        assert!(allows_without_prompt(&manager, &request));
        assert_eq!(
            decisions(&manager, &request),
            vec![StructuredPermissionDecision::Allow]
        );
        let fork = manager.fork();
        assert_eq!(
            decisions(&fork, &request),
            vec![StructuredPermissionDecision::Allow]
        );
        manager.revoke_project_permission_config_trust().unwrap();
        assert!(!manager.project_permission_config_trusted());
        assert_eq!(
            decisions(&manager, &request),
            vec![StructuredPermissionDecision::NoMatch]
        );
        manager.trust_project_permission_config().unwrap();

        let changed = PermissionManager::new_persistent_in(
            PermissionsConfig {
                project_allow_rules: vec![shell_policy_rule("git log *", Effect::Allow)],
                ..Default::default()
            },
            project,
            Arc::default(),
            state_dir,
        );
        assert!(changed.needs_project_permission_config_trust());
    }

    #[test]
    fn changing_projects_replaces_configured_policy() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("git status *", Effect::Allow)]),
            source,
        );
        let request = shell_request(&["git status --short"], workcell_shell_subject());
        assert_eq!(
            decisions(&manager, &request),
            vec![StructuredPermissionDecision::Allow]
        );

        manager.set_project_with_config(
            &destination,
            make_config(vec![shell_policy_rule("git status *", Effect::Deny)]),
        );

        assert_eq!(
            decisions(&manager, &request),
            vec![StructuredPermissionDecision::Deny]
        );
        assert_eq!(manager.project_cwd(), destination);
    }

    #[test]
    fn restrictive_project_policy_changes_invalidate_allow_trust() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let allow = shell_policy_rule("git *", Effect::Allow);
        let manager = PermissionManager::new_persistent_in(
            PermissionsConfig {
                project_allow_rules: vec![allow.clone()],
                project_restrictive_rules: vec![shell_policy_rule("git push *", Effect::Ask)],
                ..Default::default()
            },
            project.clone(),
            Arc::default(),
            state_dir.clone(),
        );
        manager.trust_project_permission_config().unwrap();

        let changed = PermissionManager::new_persistent_in(
            PermissionsConfig {
                project_allow_rules: vec![allow],
                ..Default::default()
            },
            project,
            Arc::default(),
            state_dir,
        );
        assert!(changed.needs_project_permission_config_trust());
    }

    #[test]
    fn explicit_ask_beats_default_allow_and_structured_authority() {
        let tool = ToolKey::native("platform_tool");
        let manager = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![PermissionRule {
                    tool: tool.clone(),
                    scope: Some("resource".into()),
                    effect: Effect::Ask,
                }],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        let legacy = legacy_request(&manager, tool.clone(), &["resource"]);
        assert!(coverage_with(&manager, &legacy, true, &[]).must_prompt);

        let intent = crate::tools::PermissionIntent::new(
            crate::tools::PermissionScopes::single("resource".into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: "platform".into(),
                },
                value: "resource".into(),
                access: Some(PermissionResourceAccess::Execute),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::High,
        );
        let request = PermissionRequest::from_intent(
            "ask".into(),
            tool,
            &intent,
            serde_json::json!({"value": "resource"}),
            Path::new("/tmp"),
        );
        let allow = request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        let coverage = coverage_with(&manager, &request, false, &[stored_policy(allow)]);
        assert_eq!(covered_flags(&coverage), [true]);
        assert!(coverage.must_prompt);
        assert_eq!(coverage.prompt_required, [true]);
    }

    #[test]
    fn shell_config_allow_beats_default_deny_in_production_enforcement() {
        smol::block_on(async {
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    rules: vec![
                        shell_policy_rule("*", Effect::Ask),
                        shell_policy_rule("git status *", Effect::Allow),
                    ],
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = shell_intent(&["git status --short"]);
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let result = manager
                .enforce_with_intent(
                    &ToolKey::native("shell"),
                    &intent,
                    &serde_json::json!({"command": "git status --short", "workdir": "/tmp"}),
                    &event_tx,
                    None,
                    "allow-under-deny-default",
                    &crate::CancelToken::none(),
                    None,
                    Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                    true,
                )
                .await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn shell_config_ask_beats_default_deny_in_production_enforcement() {
        smol::block_on(async {
            let manager = Arc::new(mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    rules: vec![shell_policy_rule("git status *", Effect::Ask)],
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            ));
            let intent = shell_intent(&["git status --short"]);
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_response_tx, response_rx) = flume::unbounded();
            let response_rx = Arc::new(async_lock::Mutex::new(response_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let response_rx = Arc::clone(&response_rx);
                async move {
                    manager
                        .enforce_with_intent(
                            &ToolKey::native("shell"),
                            &intent,
                            &serde_json::json!({
                                "command": "git status --short",
                                "workdir": "/tmp"
                            }),
                            &event_tx,
                            Some(&response_rx),
                            "ask-under-deny-default",
                            &crate::CancelToken::none(),
                            None,
                            Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                            true,
                        )
                        .await
                }
            });

            let event = event_rx.recv_async().await.unwrap().event;
            assert!(matches!(event, AgentEvent::PermissionRequest(_)));
            assert!(manager.answer("ask-under-deny-default", PermissionAnswer::AllowOnce));
            assert!(task.await.is_ok());
        });
    }

    #[test]
    fn empty_explicit_intent_fails_closed() {
        smol::block_on(async {
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Allow,
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("missing".into()),
                Vec::new(),
                PermissionRisk::High,
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);
            assert!(
                manager
                    .enforce_with_intent(
                        &ToolKey::native("broken"),
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "empty-intent",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        true,
                    )
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn explicit_intent_does_not_authorize_resources_from_unrelated_scopes() {
        smol::block_on(async {
            let tool = ToolKey::native("platform_tool");
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    rules: vec![PermissionRule {
                        tool: tool.clone(),
                        scope: Some("safe".into()),
                        effect: Effect::Allow,
                    }],
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("safe".into()),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: "platform".into(),
                    },
                    value: "unrelated".into(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }],
                PermissionRisk::High,
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            assert!(
                manager
                    .enforce_with_intent(
                        &tool,
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "scope-resource-drift",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        false,
                    )
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn structured_and_builtin_authority_combine_per_resource() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let external = temp.path().join("external.txt");
            std::fs::create_dir(&project).unwrap();
            let project_file = project.join("project.txt");
            let tool = ToolKey::native("file_apply_patch");
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    ..Default::default()
                },
                project.clone(),
            );
            let external_resource = filesystem_permission_resource(
                PermissionResourceKind::File,
                &external,
                PermissionResourceAccess::Write,
                &project,
            );
            let external_intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single(external.to_string_lossy().into_owned()),
                vec![external_resource.clone()],
                PermissionRisk::High,
            )
            .with_authority(PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            });
            let seed = PermissionRequest::from_intent(
                "seed".into(),
                tool.clone(),
                &external_intent,
                serde_json::json!({}),
                &project,
            );
            let rule = seed
                .option_rule("allow_exact_resources", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);

            let project_resource = filesystem_permission_resource(
                PermissionResourceKind::File,
                &project_file,
                PermissionResourceAccess::Write,
                &project,
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes {
                    scopes: vec![
                        project_file.to_string_lossy().into_owned(),
                        external.to_string_lossy().into_owned(),
                    ],
                    force_prompt: false,
                    plan_scoped: false,
                },
                vec![project_resource, external_resource],
                PermissionRisk::High,
            )
            .with_authority(PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            });
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            assert!(
                manager
                    .enforce_with_intent(
                        &tool,
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "mixed-authority",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        true,
                    )
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn project_cwd_returns_the_canonical_session_root() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let manager = mgr_with(PermissionsConfig::default(), project.clone());

        assert_eq!(manager.project_cwd(), project.canonicalize().unwrap());
    }

    #[test]
    fn explicit_intent_enforcement_uses_typed_resources_and_strict_identity() {
        smol::block_on(async {
            let manager = default_mgr();
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("legacy-scope".into()),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: "platform".into(),
                    },
                    value: "resource".into(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }],
                PermissionRisk::High,
            );
            let subject = PermissionSubject::Native {
                owner: "first-party".into(),
                contract: "platform/v1".into(),
            };
            let input = serde_json::json!({"value": "resource"});
            let request = PermissionRequest::from_intent_with_identity(
                "seed".into(),
                ToolKey::native("platform_tool"),
                &intent,
                input.clone(),
                Path::new("/tmp"),
                subject.clone(),
                PermissionExecutorKind::Native,
            );
            let rule = request
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            let allowed = manager
                .enforce_with_intent(
                    &ToolKey::native("platform_tool"),
                    &intent,
                    &input,
                    &event_tx,
                    None,
                    "matching",
                    &crate::CancelToken::none(),
                    None,
                    Some((subject, PermissionExecutorKind::Native)),
                    false,
                )
                .await;
            assert!(allowed.is_ok());

            let denied = manager
                .enforce_with_intent(
                    &ToolKey::native("platform_tool"),
                    &intent,
                    &input,
                    &event_tx,
                    None,
                    "different-contract",
                    &crate::CancelToken::none(),
                    None,
                    Some((
                        PermissionSubject::Native {
                            owner: "first-party".into(),
                            contract: "platform/v2".into(),
                        },
                        PermissionExecutorKind::Native,
                    )),
                    false,
                )
                .await;
            assert!(denied.is_err());
        });
    }

    fn plugin_edit_rule(scope: &str, effect: Effect) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("edit"),
            scope: Some(scope.into()),
            effect,
        }
    }

    #[test_case("*", "anything" => true ; "star")]
    #[test_case("cargo *", "cargo test" => true ; "prefix")]
    #[test_case("cargo *", "git push" => false ; "prefix_no_match")]
    #[test_case("pwd *", "pwd" => true ; "space_star_matches_bare_command")]
    #[test_case("pwd *", "pwd -L" => true ; "space_star_matches_with_args")]
    #[test_case("pwd *", "pwdx" => false ; "space_star_no_partial_token")]
    #[test_case("src/**", "src/main.rs" => true ; "glob")]
    #[test_case("src/**", "src/deep/nested/file.rs" => true ; "glob_deep_nested")]
    #[test_case("src/**", "src" => true ; "glob_exact_prefix")]
    #[test_case("src/**", "srcfoo" => false ; "glob_no_bare_prefix")]
    #[test_case("src/**", "other/src/main.rs" => false ; "glob_no_inner_match")]
    fn scope_match(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cd *", "cargo *"], true ; "all_allowed")]
    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cargo *"], false ; "missing_rule")]
    fn compound_check(scopes: Vec<&str>, rules: Vec<&str>, expect_allowed: bool) {
        let mgr = mgr_with(
            make_config(rules.into_iter().map(allow_rule).collect()),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&scopes, workcell_shell_subject());
        assert_eq!(allows_without_prompt(&mgr, &request), expect_allowed);
    }

    #[test]
    fn compound_denied_if_any_segment_denied() {
        let mgr = mgr_with(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(
            &["cd /tmp", "cargo test", "rm -rf /"],
            workcell_shell_subject(),
        );
        assert!(denied_by_rule(&mgr, &request));
    }

    const COMPLEX_COMMAND: &str = "echo $(whoami)";

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        smol::block_on(async {
            let mgr = mgr_with(
                make_config(vec![allow_rule("*")]),
                PathBuf::from(SHELL_WORKDIR),
            );
            let request = shell_request(&[COMPLEX_COMMAND], workcell_shell_subject());
            assert!(allows_without_prompt(&mgr, &request));
            assert!(
                enforce_shell_without_prompt(&mgr, &[COMPLEX_COMMAND], true)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn bash_workdir_scope_still_matches_legacy_command_deny() {
        let mgr = mgr_with(
            PermissionsConfig {
                yolo: true,
                rules: vec![deny_rule("git push --force")],
                ..PermissionsConfig::default()
            },
            PathBuf::from("/tmp"),
        );
        let workdir = "/tmp/project # caudra-workdir[1]=a";
        let scope = format!(
            "git push --force # caudra-workdir[{}]={workdir} # caudra-frame[{}]",
            workdir.len(),
            workdir.len(),
        );
        let request = legacy_request(&mgr, ToolKey::native("bash"), &[&scope]);
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("skill", r#"{"name":"caudra-workflow-dev"}"# => true ; "skill_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    #[test_case("bash", "echo hi" => true ; "literal_echo_allowed")]
    #[test_case("shell", "echo hi" => true ; "literal_echo_allowed_for_shell")]
    #[test_case("bash", "echo $HOME" => false ; "expanding_echo_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        let manager = default_mgr();
        let key = ToolKey::native(tool);
        let request = if is_shell_tool(&key) {
            shell_request(&[scope], workcell_shell_subject())
        } else {
            legacy_request(&manager, key, &[scope])
        };
        allows_without_prompt(&manager, &request)
    }

    #[test]
    fn builtin_echo_allow_requires_a_bundled_implementation() {
        let manager = default_mgr();
        let request = shell_request(&["echo hi"], workcell_shell_subject());

        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [true]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, false, &[])),
            [false]
        );
    }

    #[test_case(Effect::Deny; "deny")]
    #[test_case(Effect::Ask; "ask")]
    fn configured_rules_override_the_builtin_echo_allow(effect: Effect) {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("echo *", effect)]),
            PathBuf::from("/tmp"),
        );
        let request = shell_request(&["echo hi"], workcell_shell_subject());
        let rules = manager.configured_structured_rules(&request, true);

        match effect {
            Effect::Deny => assert!(
                rules
                    .iter()
                    .any(|policy| permission_rule_intersects_request(&policy.rule, &request))
            ),
            Effect::Ask => {
                let coverage = manager.request_coverage(&request, &rules, true);
                assert!(coverage.must_prompt);
                assert_eq!(coverage.prompt_required, vec![true]);
            }
            Effect::Allow => unreachable!(),
        }
    }

    #[test]
    fn builtin_echo_allow_covers_only_the_literal_command_in_a_chain() {
        let manager = default_mgr();
        let request = shell_request(&["echo hi", "rm -rf build"], workcell_shell_subject());
        let coverage = coverage_with(&manager, &request, true, &[]);

        assert_eq!(covered_flags(&coverage), vec![true, false]);
        assert!(coverage.must_prompt);
        assert_eq!(coverage.prompt_required, vec![false, true]);
        assert_eq!(coverage.rule_ask, vec![false, false]);
    }

    #[test]
    fn builtin_allows_apply_only_to_bundled_implementations() {
        let manager = default_mgr();
        let request = PermissionRequest::from_legacy(
            "task".into(),
            ToolKey::native("task"),
            vec!["{}".into()],
            serde_json::Value::Null,
            Path::new("/tmp"),
            false,
        );

        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [true]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, false, &[])),
            [false]
        );
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_resolves_symlinked_parent() {
        let tmp = std::env::temp_dir();
        let real = tmp.join("__caudra_test_scope_symlink_real");
        let link = tmp.join("__caudra_test_scope_symlink_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let pattern = format!("{}/**", real.display());
        let value = format!("{}/new_file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent should resolve: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn scope_matches_relative_pattern_before_dir_exists() {
        // A relative rule like `dist/**` must match an absolute value even
        // before the directory exists.
        let cwd = std::env::current_dir().unwrap();
        let value = cwd.join("__caudra_nonexistent_dist/file.txt");
        assert!(!value.exists(), "test dir must not exist");
        assert!(
            scope_matches("__caudra_nonexistent_dist/**", &value.to_string_lossy()),
            "relative pattern should match absolute value: value={}",
            value.display()
        );
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_symlinked_parent_with_nonexistent_tail() {
        // Regression: symlinked leading component plus a non-existent tail
        // (`proj`). Both sides must resolve the symlink before appending the
        // lexical tail, else the prefix stays lexical and this returns false.
        let tmp = std::env::temp_dir();
        let real = tmp.join("__caudra_test_scope_symlink_tail_real");
        let link = tmp.join("__caudra_test_scope_symlink_tail_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // `proj` under the symlink does not exist.
        let pattern = format!("{}/proj/**", link.display());
        let value = format!("{}/proj/file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent with non-existent tail should match: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn path_traversal_prompts() {
        let path = normalize_scope_path("/tmp/../etc/passwd");
        let manager = default_mgr();
        let request = legacy_request(&manager, ToolKey::native("write"), &[&path]);
        assert!(!denied_by_rule(&manager, &request));
        assert!(!allows_without_prompt(&manager, &request));
    }

    #[test]
    fn deny_overrides_default_allow() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![deny_rule("rm *")],
                ..Default::default()
            },
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&["rm -rf /"], workcell_shell_subject());
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn boundary_inside_proceeds() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp.clone());
        assert!(
            mgr.boundary_block_reason(&tmp.join("some_file.txt"))
                .is_none()
        );
    }

    #[test]
    fn boundary_outside_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp);
        #[cfg(unix)]
        let outside = Path::new("/etc/hosts");
        #[cfg(windows)]
        let outside = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
        assert!(mgr.boundary_block_reason(outside).is_none());
    }

    #[test]
    fn boundary_dotdot_smuggling_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let sub = tmp.join("__caudra_test_boundary");
        std::fs::create_dir_all(&sub).unwrap();
        #[cfg(unix)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("etc")
            .join("passwd");
        #[cfg(windows)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("Windows")
            .join("System32");
        let mgr = mgr_with(PermissionsConfig::default(), sub.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-cwd dotdot path should prompt, not hard-block: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    #[cfg(unix)]
    fn boundary_symlink_escape_proceeds_via_prompt() {
        // Lexical normalization resolves this inside (/project/escape), but
        // incremental canonicalization follows the symlink first, so `..`
        // escapes outside. The permission prompt catches it, not this function.
        let tmp = std::env::temp_dir();
        let project = tmp.join("__caudra_test_symlink_escape");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let link = project.join("link");
        let _ = std::os::unix::fs::symlink(&tmp, &link);

        let attack = link.join("..").join("escape_target");
        let mgr = mgr_with(PermissionsConfig::default(), project.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-boundary edits are gated by the prompt, not hard-blocked: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn boundary_nonexistent_cwd_proceeds_via_lexical_tail() {
        let missing = std::env::temp_dir().join("__caudra_test_absent_cwd_xyz");
        let _ = std::fs::remove_dir_all(&missing);
        let mgr = mgr_with(PermissionsConfig::default(), missing.clone());
        assert!(
            mgr.boundary_block_reason(&missing.join("file.txt"))
                .is_none()
        );
    }

    #[test]
    fn permission_answer_roundtrip() {
        for a in [
            PermissionAnswer::AllowOnce,
            PermissionAnswer::AllowSession,
            PermissionAnswer::AllowAlwaysLocal,
            PermissionAnswer::AllowOption {
                option_id: "allow_url_origin".into(),
                lifetime: PermissionLifetime::Project,
            },
            PermissionAnswer::AllowComposed {
                rows: vec![
                    Some(PermissionRowGrant::Offered("command_exact_0".into())),
                    Some(PermissionRowGrant::Written("git status *".into())),
                    None,
                ],
                lifetime: PermissionLifetime::Conversation,
            },
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("hint".into()),
        ] {
            assert_eq!(PermissionAnswer::decode(&a.encode()), Some(a));
        }
    }

    const ALLOWED_COMMANDS: [&str; 2] = ["cargo test", "git push"];

    #[test]
    fn force_prompt_skips_allow_rules() {
        smol::block_on(async {
            let mgr = mgr_with(
                make_config(vec![allow_rule("cargo *"), allow_rule("git *")]),
                PathBuf::from(SHELL_WORKDIR),
            );
            let request = shell_request(&ALLOWED_COMMANDS, workcell_shell_subject());
            assert!(allows_without_prompt(&mgr, &request));
            assert!(
                enforce_shell_without_prompt(&mgr, &ALLOWED_COMMANDS, false)
                    .await
                    .is_ok()
            );
            assert!(
                enforce_shell_without_prompt(&mgr, &ALLOWED_COMMANDS, true)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn deny_wins_over_force_prompt() {
        let mgr = mgr_with(
            make_config(vec![deny_rule("rm *")]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let mut forced = shell_intent(&["rm -rf /"]);
        forced.scopes.force_prompt = true;
        let request = PermissionRequest::from_intent_with_identity(
            "forced-request".into(),
            ToolKey::native("shell"),
            &forced,
            serde_json::json!({"command": "rm -rf /", "workdir": SHELL_WORKDIR}),
            Path::new(SHELL_WORKDIR),
            workcell_shell_subject(),
            PermissionExecutorKind::Native,
        );
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn partial_coverage_prompts_only_the_uncovered_commands() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&["cargo test", "git push", "ls"], workcell_shell_subject());
        let coverage = coverage_with(&mgr, &request, true, &[]);
        assert_eq!(covered_flags(&coverage), vec![true, false, false]);
        assert!(coverage.must_prompt);
    }

    #[test]
    fn mcp_prompt_preserves_input_larger_than_200_bytes() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({
                "query": "x".repeat(512),
                "nested": {"z": 1, "a": 2}
            });
            let scope = canonical_json(&input);
            assert!(scope.len() > 200);
            let scopes = crate::tools::PermissionScopes::single(scope.clone());
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let input = input.clone();
                let scopes = scopes.clone();
                let answer_rx = Arc::clone(&answer_rx);
                async move {
                    manager
                        .enforce(
                            &ToolKey::parse("server.lookup").unwrap(),
                            &scopes,
                            &input,
                            &event_tx,
                            Some(&answer_rx),
                            "request-id",
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                }
            });
            let event = event_rx.recv_async().await.unwrap().event;
            assert!(manager.answer("request-id", PermissionAnswer::Deny));
            let result = task.await;
            assert!(result.is_err());

            let AgentEvent::PermissionRequest(request) = event else {
                panic!("expected permission request, got {event:?}");
            };
            assert_eq!(request.input, input);
            assert_eq!(request.scopes, [scope]);
            assert_eq!(request.input_digest, canonical_json_sha256(&request.input));
            assert!(request.scopes[0].len() > 200);
            assert!(request.options.iter().any(|option| option.broad));
            assert!(
                request
                    .options
                    .iter()
                    .filter(|option| option.broad)
                    .all(|option| !option.is_default)
            );
        });
    }

    #[test]
    fn duplicate_request_id_does_not_replace_the_pending_request() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let scopes = crate::tools::PermissionScopes::single("cargo test".into());
            let input = serde_json::json!({"command": "cargo test"});
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));
            let start = |manager: Arc<PermissionManager>| {
                let scopes = scopes.clone();
                let input = input.clone();
                let event_tx = event_tx.clone();
                let answer_rx = Arc::clone(&answer_rx);
                smol::spawn(async move {
                    manager
                        .enforce(
                            &ToolKey::native("bash"),
                            &scopes,
                            &input,
                            &event_tx,
                            Some(&answer_rx),
                            "duplicate-id",
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                })
            };

            let first = start(Arc::clone(&manager));
            event_rx.recv_async().await.unwrap();
            let second = start(Arc::clone(&manager));
            assert!(second.await.is_err());
            assert_eq!(manager.pending_count(), 1);
            assert!(manager.answer("duplicate-id", PermissionAnswer::Deny));
            assert!(first.await.is_err());
            assert_eq!(manager.pending_count(), 0);
        });
    }

    fn pending_tool_enforcement(
        manager: Arc<PermissionManager>,
        request_id: &str,
        tool: &str,
        scope: String,
        input: serde_json::Value,
    ) -> (
        smol::Task<Result<(), PermissionError>>,
        flume::Receiver<crate::Envelope>,
    ) {
        pending_scope_enforcement(
            manager,
            request_id,
            tool,
            crate::tools::PermissionScopes::single(scope),
            input,
        )
    }

    fn pending_scope_enforcement(
        manager: Arc<PermissionManager>,
        request_id: &str,
        tool: &str,
        scopes: crate::tools::PermissionScopes,
        input: serde_json::Value,
    ) -> (
        smol::Task<Result<(), PermissionError>>,
        flume::Receiver<crate::Envelope>,
    ) {
        let (event_tx, event_rx) = flume::unbounded();
        let event_tx = crate::EventSender::new(event_tx, 0);
        let request_id = request_id.to_owned();
        let tool = ToolKey::native(tool);
        let task = smol::spawn(async move {
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = async_lock::Mutex::new(legacy_rx);
            manager
                .enforce(
                    &tool,
                    &scopes,
                    &input,
                    &event_tx,
                    Some(&legacy_rx),
                    &request_id,
                    &crate::CancelToken::none(),
                    None,
                )
                .await
        });
        (task, event_rx)
    }

    #[test]
    fn reusable_exact_approval_resolves_covered_parallel_requests() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert_eq!(manager.pending_count(), 2);
            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
            assert_eq!(manager.pending_count(), 0);
            assert!(matches!(
                second_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved {
                    request_id,
                    source_request_id
                } if request_id == "second" && source_request_id == "first"
            ));
        });
    }

    #[test]
    fn reusable_approval_does_not_resolve_configured_ask_prompts() {
        smol::block_on(async {
            let manager = Arc::new(mgr_with(
                make_config(vec![PermissionRule {
                    tool: ToolKey::native("bash"),
                    scope: Some("cargo test".into()),
                    effect: Effect::Ask,
                }]),
                PathBuf::from("/tmp"),
            ));
            let input = serde_json::json!({"command": "cargo test"});
            let seed = PermissionRequest::from_legacy(
                "seed".into(),
                ToolKey::native("bash"),
                vec!["cargo test".into()],
                input.clone(),
                Path::new("/tmp"),
                false,
            );
            let rule = seed
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(second_events.is_empty());
            assert!(manager.answer("second", PermissionAnswer::AllowOnce));
            assert!(second.await.is_ok());
        });
    }

    const BROAD_SHELL_OPTION: &str = "allow_any_command";

    fn broad_shell_grant() -> PermissionAnswer {
        PermissionAnswer::AllowOption {
            option_id: BROAD_SHELL_OPTION.into(),
            lifetime: PermissionLifetime::Conversation,
        }
    }

    /// The batch case: several commands ask at once and one broad grant answers
    /// them all. Every other sweep test uses the same command twice, so nothing
    /// caught that an uncovered command was marked un-sweepable for good.
    #[test]
    fn a_broad_authority_sweeps_a_pending_sibling_with_a_different_command() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "rm -rf build".into(),
                serde_json::json!({"command": "rm -rf build"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            assert_eq!(manager.pending_count(), 2);

            // `answer` sweeps and notifies under its own lock, so both are
            // observable the moment it returns. Asserting before awaiting keeps
            // a regression a failure rather than a hang on a task nobody freed.
            assert!(manager.answer("first", broad_shell_grant()));
            assert_eq!(manager.pending_count(), 0);
            assert!(matches!(
                second_events.try_recv().unwrap().event,
                AgentEvent::PermissionRequestResolved {
                    request_id,
                    source_request_id
                } if request_id == "second" && source_request_id == "first"
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
        });
    }

    /// A per-command answer deliberately says nothing about commands that were
    /// already allowed, so the sweep has to count a sibling's own prior
    /// coverage. Otherwise narrowing an answer to what was actually undecided
    /// would strand every sibling that shares an allowed command.
    #[test]
    fn a_narrow_authority_sweeps_a_sibling_whose_other_commands_were_already_allowed() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let settled = "cargo build";
            let undecided = "npm test";
            let seed = PermissionRequest::from_legacy(
                "seed".into(),
                ToolKey::native("bash"),
                vec![settled.into()],
                serde_json::json!({"command": settled}),
                Path::new("/tmp"),
                false,
            );
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(
                    seed.option_rule("allow_exact_commands", PermissionLifetime::Conversation)
                        .unwrap(),
                )
                .unwrap(),
            ]);
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                undecided.into(),
                serde_json::json!({"command": undecided}),
            );
            let (second, second_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                crate::tools::PermissionScopes {
                    scopes: vec![settled.into(), undecided.into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                serde_json::json!({"command": format!("{settled} && {undecided}")}),
            );
            first_events.recv_async().await.unwrap();
            let AgentEvent::PermissionRequest(sibling) =
                second_events.recv_async().await.unwrap().event
            else {
                panic!("{COMPOSED_PROMPT_MISSING}");
            };
            assert!(sibling.presentation.resources[0].covered());
            assert!(!sibling.presentation.resources[1].covered());

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowComposed {
                    rows: vec![Some(PermissionRowGrant::Offered("command_exact_0".into()))],
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert_eq!(manager.pending_count(), 0);
            assert!(matches!(
                second_events.try_recv().unwrap().event,
                AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == "second"
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
        });
    }

    /// A composed answer files one rule per command, so a sibling waiting on
    /// the second of them is only swept if the whole set is consulted.
    #[test]
    fn a_sibling_is_swept_by_any_rule_the_answer_filed() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let first_command = "cargo build";
            let second_command = "npm test";
            let (first, first_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                crate::tools::PermissionScopes {
                    scopes: vec![first_command.into(), second_command.into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                serde_json::json!({"command": format!("{first_command} && {second_command}")}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                second_command.into(),
                serde_json::json!({"command": second_command}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            assert_eq!(manager.pending_count(), 2);

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowComposed {
                    rows: vec![
                        Some(PermissionRowGrant::Offered("command_exact_0".into())),
                        Some(PermissionRowGrant::Offered("command_exact_1".into())),
                    ],
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert_eq!(manager.pending_count(), 0);
            assert!(matches!(
                second_events.try_recv().unwrap().event,
                AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == "second"
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
        });
    }

    /// A forced prompt is the mode asking, not the rules, so no grant answers it.
    #[test]
    fn a_broad_authority_leaves_a_forced_prompt_pending() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                crate::tools::PermissionScopes::force_prompt("rm -rf build".into()),
                serde_json::json!({"command": "rm -rf build"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", broad_shell_grant()));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(second_events.is_empty());
            assert!(manager.answer("second", PermissionAnswer::AllowOnce));
            assert!(second.await.is_ok());
        });
    }

    #[test]
    fn allow_once_resolves_only_the_selected_parallel_request() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowOnce));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(second_events.is_empty());
            assert!(manager.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn reusable_exact_approval_leaves_different_input_pending() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo check".into(),
                serde_json::json!({"command": "cargo check"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(second_events.is_empty());
            assert!(manager.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn conversation_approval_does_not_cross_manager_forks() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let other = Arc::new(manager.fork());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&other),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(other.pending_count(), 1);
            assert!(second_events.is_empty());
            assert!(other.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn filesystem_subtree_approval_resolves_descendant_reads_only() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let external = temp.path().join("external");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&external).unwrap();
            let manager = Arc::new(mgr_with(PermissionsConfig::default(), project));
            let first_path = external.join("first.txt").to_string_lossy().into_owned();
            let second_path = external
                .join("nested/second.txt")
                .to_string_lossy()
                .into_owned();
            let outside_path = temp
                .path()
                .join("outside.txt")
                .to_string_lossy()
                .into_owned();
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "file_read",
                first_path.clone(),
                serde_json::json!({"path": first_path}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "file_read",
                second_path.clone(),
                serde_json::json!({"path": second_path}),
            );
            let (outside, outside_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "outside",
                "file_read",
                outside_path.clone(),
                serde_json::json!({"path": outside_path}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            outside_events.recv_async().await.unwrap();

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowOption {
                    option_id: "allow_filesystem_subtree".into(),
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(manager.answer("outside", PermissionAnswer::Deny));
            assert!(outside.await.is_err());
        });
    }

    #[test]
    fn deny_rule_with_none_scope_blocks_everything() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("bash"),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&["anything"], workcell_shell_subject());
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn wildcard_deny_blocks_all_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        // Any deny wins: Wildcard deny blocks everything including builtins
        let command = shell_request(&["ls"], workcell_shell_subject());
        let write = legacy_request(&mgr, ToolKey::native("write"), &["/tmp/x"]);
        assert!(denied_by_rule(&mgr, &command));
        assert!(denied_by_rule(&mgr, &write));
    }

    #[test]
    fn yolo_mode_allows_but_deny_still_blocks() {
        smol::block_on(async {
            let mgr = mgr_with(
                make_config(vec![deny_rule("rm *")]),
                PathBuf::from(SHELL_WORKDIR),
            );
            mgr.toggle_yolo();
            assert!(mgr.is_yolo());
            assert!(
                enforce_shell_without_prompt(&mgr, &["cargo test"], false)
                    .await
                    .is_ok()
            );
            let denied = shell_request(&["rm -rf /"], workcell_shell_subject());
            assert!(denied_by_rule(&mgr, &denied));
            assert!(
                enforce_shell_without_prompt(&mgr, &["rm -rf /"], false)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn remote_policy_replacement_removes_authority_and_fails_closed() {
        smol::block_on(async {
            const FIRST_DIGEST: &str =
                "1111111111111111111111111111111111111111111111111111111111111111";
            const SECOND_DIGEST: &str =
                "2222222222222222222222222222222222222222222222222222222222222222";
            let temp = tempfile::tempdir().unwrap();
            let manager = PermissionManager::new_persistent_in(
                PermissionsConfig {
                    yolo: true,
                    ..Default::default()
                },
                temp.path().to_path_buf(),
                Arc::default(),
                StateDir::from_path(temp.path().join("state")),
            );
            let first = remote_permission_asset("revision-1", FIRST_DIGEST, "trusted-first");
            manager
                .replace_remote_permission_asset(Some(&first))
                .unwrap();
            manager.trust_project_permission_config().unwrap();
            assert!(manager.active_policy().iter().any(|entry| {
                entry.rule.scope.as_deref() == Some("trusted-first")
                    && entry.rule.effect == Effect::Allow
            }));

            let second = remote_permission_asset("revision-2", SECOND_DIGEST, "untrusted-second");
            const PERSISTENCE_FAILED: &str = "injected persistence failure";
            assert_eq!(
                manager
                    .replace_remote_permission_asset_after(Some(&second), || Err(
                        PERSISTENCE_FAILED.to_owned()
                    ))
                    .unwrap_err(),
                PERSISTENCE_FAILED
            );
            assert!(
                manager
                    .active_policy()
                    .iter()
                    .any(|entry| entry.rule.scope.as_deref() == Some("trusted-first")
                        && entry.rule.effect == Effect::Allow)
            );
            manager
                .replace_remote_permission_asset(Some(&second))
                .unwrap();
            assert!(!manager.active_policy().iter().any(|entry| {
                matches!(
                    entry.rule.scope.as_deref(),
                    Some("trusted-first" | "untrusted-second")
                ) && entry.rule.effect == Effect::Allow
            }));
            assert!(manager.needs_project_permission_config_trust());

            manager.replace_remote_permission_asset(None).unwrap();
            assert!(!manager.needs_project_permission_config_trust());
            assert!(!manager.active_policy().iter().any(|entry| {
                matches!(
                    entry.rule.scope.as_deref(),
                    Some("remote-denied" | "trusted-first" | "untrusted-second")
                )
            }));

            manager
                .replace_remote_permission_asset(Some(&first))
                .unwrap();
            let mut invalid = second;
            let other_authority = AuthorityIdentity::new(
                SourceTrustAnchor::new("https://other.example").unwrap(),
                "server",
                "workspace",
                "generation",
                "namespace",
            )
            .unwrap();
            invalid.source.project =
                ProjectIdentity::new(other_authority, ProjectKey::new("project").unwrap());
            assert!(
                manager
                    .replace_remote_permission_asset(Some(&invalid))
                    .is_err()
            );
            assert!(!manager.active_policy().iter().any(|entry| {
                matches!(
                    entry.rule.scope.as_deref(),
                    Some("remote-denied" | "trusted-first")
                )
            }));
            assert!(
                enforce_shell_without_prompt(&manager, &["echo still denied"], false)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn remote_default_denial_is_final_under_yolo() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let manager = PermissionManager::new_persistent_in(
                PermissionsConfig {
                    yolo: true,
                    ..Default::default()
                },
                temp.path().to_path_buf(),
                Arc::default(),
                StateDir::from_path(temp.path().join("state")),
            );
            let mut asset = remote_permission_asset(
                "revision",
                "3333333333333333333333333333333333333333333333333333333333333333",
                "unused",
            );
            asset.declarations.restrictive_rules.clear();
            asset.declarations.allow_rules.clear();
            asset.declarations.default = Some(DefaultEffect::Deny);
            manager
                .replace_remote_permission_asset(Some(&asset))
                .unwrap();

            assert!(
                enforce_shell_without_prompt(&manager, &["echo denied by server"], false)
                    .await
                    .is_err()
            );
        });
    }

    fn seeded_mgr(yolo: bool) -> PermissionManager {
        mgr_with(
            PermissionsConfig {
                yolo,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        )
    }

    /// A fork runs the same session, so it has to answer both questions the
    /// same way or a respawned agent drifts from the tab that owns it.
    fn yolo_state(mgr: &PermissionManager) -> (bool, Option<bool>) {
        let forked = mgr.fork();
        assert_eq!(
            (forked.is_yolo(), forked.persisted_yolo()),
            (mgr.is_yolo(), mgr.persisted_yolo()),
        );
        (mgr.is_yolo(), mgr.persisted_yolo())
    }

    /// A stored intent replaces the seed outright, and no stored intent falls
    /// back to it: `--yolo` must neither be erased by an untouched session nor
    /// survive one the user explicitly turned off.
    #[test_case(false, None        => (false, None)        ; "no_flag_and_no_intent_stays_off")]
    #[test_case(true,  None        => (true,  None)        ; "the_flag_applies_but_is_never_stored")]
    #[test_case(false, Some(true)  => (true,  Some(true))  ; "stored_on_comes_back_without_the_flag")]
    #[test_case(true,  Some(true)  => (true,  Some(true))  ; "the_flag_does_not_wipe_stored_on")]
    #[test_case(true,  Some(false) => (false, Some(false)) ; "stored_off_overrides_the_flag")]
    #[test_case(false, Some(false) => (false, Some(false)) ; "stored_off_stays_off")]
    fn a_stored_yolo_intent_replaces_the_seed(
        seed: bool,
        stored: Option<bool>,
    ) -> (bool, Option<bool>) {
        let mgr = seeded_mgr(seed);
        mgr.set_session_yolo(stored);
        yolo_state(&mgr)
    }

    /// `/yolo` always drives the effective state, so under `--yolo` it can turn
    /// the session off, and either way the session now owns the answer.
    #[test_case(false => (true,  Some(true))  ; "toggling_on_claims_the_session")]
    #[test_case(true  => (false, Some(false)) ; "toggling_off_under_the_flag_claims_the_session")]
    fn toggling_yolo_records_the_intent(seed: bool) -> (bool, Option<bool>) {
        let mgr = seeded_mgr(seed);
        assert_eq!(mgr.toggle_yolo(), !seed);
        yolo_state(&mgr)
    }

    const CARGO_TEST_COMMAND: &str = "cargo test";

    #[test]
    fn default_deny_blocks_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                ..Default::default()
            },
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&[CARGO_TEST_COMMAND], workcell_shell_subject());
        assert!(denied_by_default(&mgr, &request));
    }

    #[test]
    fn default_deny_with_allow_rules() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                rules: vec![allow_rule("cargo *")],
                ..Default::default()
            },
            PathBuf::from(SHELL_WORKDIR),
        );
        let allowed = shell_request(&[CARGO_TEST_COMMAND], workcell_shell_subject());
        let denied = shell_request(&["rm -rf /"], workcell_shell_subject());
        assert!(allows_without_prompt(&mgr, &allowed));
        assert!(denied_by_default(&mgr, &denied));
    }

    #[test]
    fn default_allow_allows_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                ..Default::default()
            },
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&[CARGO_TEST_COMMAND], workcell_shell_subject());
        assert!(!denied_by_rule(&mgr, &request));
        assert!(allowed_by_default(&mgr, &request));
    }

    #[test]
    fn default_prompt_is_default_behavior() {
        let mgr = mgr_with(PermissionsConfig::default(), PathBuf::from(SHELL_WORKDIR));
        let request = shell_request(&[CARGO_TEST_COMMAND], workcell_shell_subject());
        assert!(!allows_without_prompt(&mgr, &request));
        assert!(matches!(
            mgr.default_effect(&request.tool),
            DefaultEffect::Prompt
        ));
    }

    const EMPTY_MCP_SCOPE: &str = "{}";

    #[test]
    fn mcp_server_wildcard_matches_all_server_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        for tool in ["search", "web_search"] {
            let request = legacy_request(
                &mgr,
                ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: tool.into(),
                },
                &[EMPTY_MCP_SCOPE],
            );
            assert!(allows_without_prompt(&mgr, &request));
        }
    }

    #[test]
    fn mcp_server_wildcard_does_not_match_other_server() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        let request = legacy_request(
            &mgr,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "search".into(),
            },
            &[EMPTY_MCP_SCOPE],
        );
        assert!(!allows_without_prompt(&mgr, &request));
    }

    #[test]
    fn per_tool_default_overrides_global() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                tool_defaults: HashMap::from([(ToolKey::native("bash"), DefaultEffect::Allow)]),
                rules: vec![],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        let command = legacy_request(&mgr, ToolKey::native("bash"), &[CARGO_TEST_COMMAND]);
        let write = legacy_request(&mgr, ToolKey::native("write"), &["/etc/passwd"]);
        assert!(allowed_by_default(&mgr, &command));
        assert!(denied_by_default(&mgr, &write));
    }

    const PLAN_PATH: &str =
        "/home/user/.local/state/caudra/projects/app-0123456789abcdef/plans/test.md";

    /// Enforces against the plan being built with no response channel, so the
    /// plan-write escape hatch is the only thing that can let the call through.
    async fn enforce_plan_write_without_prompt(
        manager: &PermissionManager,
        tool: &str,
        scopes: &[&str],
    ) -> Result<(), PermissionError> {
        let (event_tx, _event_rx) = flume::unbounded();
        manager
            .enforce(
                &ToolKey::native(tool),
                &crate::tools::PermissionScopes {
                    scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                    force_prompt: false,
                    plan_scoped: false,
                },
                &serde_json::Value::Null,
                &crate::EventSender::new(event_tx, 0),
                None,
                "plan-request",
                &crate::CancelToken::none(),
                Some(Path::new(PLAN_PATH)),
            )
            .await
    }

    #[test_case("write", true ; "write_tool_allowed")]
    #[test_case("edit", true ; "edit_tool_allowed")]
    #[test_case("bash", false ; "non_write_tool_prompts")]
    fn plan_path_auto_allows_file_write_tools_only(tool: &str, expect_allowed: bool) {
        smol::block_on(async {
            let mgr = default_mgr();
            assert_eq!(
                enforce_plan_write_without_prompt(&mgr, tool, &[PLAN_PATH])
                    .await
                    .is_ok(),
                expect_allowed,
            );
        });
    }

    const PLUGIN_EDIT_PATH: &str = "/x/f";

    #[test]
    fn plugin_rules_apply_to_manager_and_forks() {
        let store = Arc::new(PluginRuleStore::default());
        let mgr = PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            Arc::clone(&store),
        );
        let fork = mgr.fork();
        store.replace("memory", vec![plugin_edit_rule("/x/**", Effect::Allow)]);
        for manager in [&mgr, &fork] {
            let request = legacy_request(manager, ToolKey::native("edit"), &[PLUGIN_EDIT_PATH]);
            assert!(allows_without_prompt(manager, &request));
        }
    }

    #[test]
    fn config_deny_beats_plugin_allow() {
        let store = Arc::new(PluginRuleStore::default());
        store.replace("memory", vec![plugin_edit_rule("/x/**", Effect::Allow)]);
        let mgr = PermissionManager::new_nonpersistent(
            make_config(vec![plugin_edit_rule("/x/**", Effect::Deny)]),
            PathBuf::from("/tmp"),
            store,
        );
        let request = legacy_request(&mgr, ToolKey::native("edit"), &[PLUGIN_EDIT_PATH]);
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn plan_path_multi_scope_all_must_match() {
        smol::block_on(async {
            let mgr = default_mgr();
            assert!(
                enforce_plan_write_without_prompt(&mgr, "write", &[PLAN_PATH, PLAN_PATH])
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_write_without_prompt(&mgr, "write", &[PLAN_PATH, "/etc/passwd"])
                    .await
                    .is_err()
            );
        });
    }

    fn persistent_manager(state_dir: StateDir, project: &Path) -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig::default(),
            project.to_path_buf(),
            Arc::default(),
            state_dir,
        ))
    }

    #[test]
    fn project_approval_resolves_only_pending_requests_in_the_same_project() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let manager = persistent_manager(
                StateDir::from_path(temp.path().join("state")),
                &first_project,
            );
            let same_project = Arc::new(manager.fork());
            let other_project = Arc::new(manager.fork());
            other_project.set_project(&second_project);
            let url = "https://example.com/docs";
            let input = serde_json::json!({"url": url});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (same, same_events) = pending_tool_enforcement(
                Arc::clone(&same_project),
                "same",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (other, other_events) = pending_tool_enforcement(
                Arc::clone(&other_project),
                "other",
                "webfetch",
                url.into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            same_events.recv_async().await.unwrap();
            other_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowAlwaysLocal));
            assert!(first.await.is_ok());
            assert!(same.await.is_ok());
            assert_eq!(other_project.pending_count(), 1);
            assert!(other_events.is_empty());
            assert!(other_project.answer("other", PermissionAnswer::Deny));
            assert!(other.await.is_err());
        });
    }

    #[test]
    fn global_approval_resolves_pending_requests_in_other_projects() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let manager = persistent_manager(
                StateDir::from_path(temp.path().join("state")),
                &first_project,
            );
            let other_project = Arc::new(manager.fork());
            other_project.set_project(&second_project);
            let url = "https://example.com/docs";
            let input = serde_json::json!({"url": url});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (other, other_events) = pending_tool_enforcement(
                Arc::clone(&other_project),
                "other",
                "webfetch",
                url.into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            other_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowAlwaysGlobal));
            assert!(first.await.is_ok());
            assert!(other.await.is_ok());
            assert_eq!(other_project.pending_count(), 0);
            assert!(matches!(
                other_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == "other"
            ));
        });
    }

    async fn answer_enforcement(
        manager: Arc<PermissionManager>,
        scope: &str,
        input: serde_json::Value,
        answer: PermissionAnswer,
    ) -> Result<(), PermissionError> {
        answer_tool_enforcement(manager, "bash", scope, input, answer).await
    }

    async fn answer_tool_enforcement(
        manager: Arc<PermissionManager>,
        tool: &str,
        scope: &str,
        input: serde_json::Value,
        answer: PermissionAnswer,
    ) -> Result<(), PermissionError> {
        let scopes = crate::tools::PermissionScopes::single(scope.to_owned());
        let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
        let event_tx = crate::EventSender::new(event_tx, 0);
        let (_legacy_tx, legacy_rx) = flume::unbounded();
        let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
        let tool = ToolKey::native(tool);
        let task = smol::spawn({
            let manager = Arc::clone(&manager);
            let legacy_rx = Arc::clone(&legacy_rx);
            async move {
                manager
                    .enforce(
                        &tool,
                        &scopes,
                        &input,
                        &event_tx,
                        Some(&legacy_rx),
                        "durable-request",
                        &crate::CancelToken::none(),
                        None,
                    )
                    .await
            }
        });
        let event = event_rx.recv_async().await.unwrap().event;
        assert!(matches!(event, AgentEvent::PermissionRequest(_)));
        assert!(manager.answer("durable-request", answer));
        task.await
    }

    async fn enforce_without_prompt(
        manager: &PermissionManager,
        scope: &str,
        input: serde_json::Value,
    ) -> Result<(), PermissionError> {
        enforce_tool_without_prompt(manager, "bash", scope, input).await
    }

    async fn enforce_tool_without_prompt(
        manager: &PermissionManager,
        tool: &str,
        scope: &str,
        input: serde_json::Value,
    ) -> Result<(), PermissionError> {
        let (event_tx, _) = flume::unbounded::<crate::Envelope>();
        manager
            .enforce(
                &ToolKey::native(tool),
                &crate::tools::PermissionScopes::single(scope.to_owned()),
                &input,
                &crate::EventSender::new(event_tx, 0),
                None,
                "restart-request",
                &crate::CancelToken::none(),
                None,
            )
            .await
    }

    const COMPOSED_REQUEST_ID: &str = "composed-request";
    const COMPOSED_PROMPT_MISSING: &str = "batched commands did not raise a prompt";

    /// The point of per-row scopes: one answer, one rule, and only the rows
    /// that asked to be remembered end up in it. The row that asked for nothing
    /// still runs, because the call proceeds on the answer and not on the rule.
    #[test]
    fn a_composed_answer_remembers_only_the_rows_that_asked_for_it() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let commands = ["git status --short", "cargo test"];
            let input = serde_json::json!({"command": commands.join(" && ")});
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let input = input.clone();
                async move {
                    manager
                        .enforce(
                            &ToolKey::native("bash"),
                            &crate::tools::PermissionScopes {
                                scopes: commands.iter().map(|c| (*c).to_owned()).collect(),
                                force_prompt: false,
                                plan_scoped: false,
                            },
                            &input,
                            &crate::EventSender::new(event_tx, 0),
                            Some(&legacy_rx),
                            COMPOSED_REQUEST_ID,
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                }
            });
            assert!(matches!(
                event_rx.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequest(_)
            ));
            assert!(manager.answer(
                COMPOSED_REQUEST_ID,
                PermissionAnswer::AllowComposed {
                    rows: vec![
                        Some(PermissionRowGrant::Offered("command_pattern_0".into())),
                        None,
                    ],
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert!(task.await.is_ok());

            assert!(
                enforce_without_prompt(
                    &manager,
                    "git status --porcelain",
                    serde_json::json!({"command": "git status --porcelain"})
                )
                .await
                .is_ok()
            );
            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo test",
                    serde_json::json!({"command": "cargo test"})
                )
                .await
                .is_err()
            );
        });
    }

    fn opaque_intent(
        command: &str,
        workdir: &Path,
        plan_scoped: bool,
    ) -> crate::tools::PermissionIntent {
        let scopes = crate::tools::PermissionScopes {
            scopes: vec![command.to_owned()],
            force_prompt: false,
            plan_scoped,
        };
        crate::tools::PermissionIntent::new(
            scopes,
            vec![PermissionResource {
                kind: PermissionResourceKind::Command,
                value: command.into(),
                access: Some(PermissionResourceAccess::Execute),
                protected: true,
                requires_prompt: true,
                attributes: BTreeMap::from([(
                    "workdir".into(),
                    workdir.to_string_lossy().into_owned(),
                )]),
            }],
            PermissionRisk::Critical,
        )
        .with_authority(PermissionAuthorityProfile::Shell)
    }

    async fn enforce_opaque_command_without_prompt(
        manager: &PermissionManager,
        workdir: &Path,
        command: &str,
    ) -> Result<(), PermissionError> {
        enforce_opaque_command_scoped(manager, workdir, command, false).await
    }

    async fn enforce_plan_command_without_prompt(
        manager: &PermissionManager,
        workdir: &Path,
        command: &str,
    ) -> Result<(), PermissionError> {
        enforce_opaque_command_scoped(manager, workdir, command, true).await
    }

    async fn enforce_opaque_command_scoped(
        manager: &PermissionManager,
        workdir: &Path,
        command: &str,
        plan_scoped: bool,
    ) -> Result<(), PermissionError> {
        let (event_tx, _event_rx) = flume::unbounded::<crate::Envelope>();
        manager
            .enforce_with_intent(
                &ToolKey::native("bash"),
                &opaque_intent(command, workdir, plan_scoped),
                &serde_json::json!({"command": command}),
                &crate::EventSender::new(event_tx, 0),
                None,
                "opaque-request",
                &crate::CancelToken::none(),
                None,
                None,
                true,
            )
            .await
    }

    /// Answers one plan-scoped command prompt, reporting whether the answer was
    /// accepted and what the request offered. A refused answer leaves the call
    /// waiting, so it is always followed by one that ends it.
    async fn answer_plan_command(
        manager: Arc<PermissionManager>,
        workdir: &Path,
        command: &str,
        answer: PermissionAnswer,
    ) -> (bool, Result<(), PermissionError>, Box<PermissionRequest>) {
        let intent = opaque_intent(command, workdir, true);
        let input = serde_json::json!({ "command": command });
        let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
        let (_legacy_tx, legacy_rx) = flume::unbounded();
        let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
        let task = smol::spawn({
            let manager = Arc::clone(&manager);
            let input = input.clone();
            async move {
                manager
                    .enforce_with_intent(
                        &ToolKey::native("bash"),
                        &intent,
                        &input,
                        &crate::EventSender::new(event_tx, 0),
                        Some(&legacy_rx),
                        PLAN_REQUEST_ID,
                        &crate::CancelToken::none(),
                        None,
                        None,
                        true,
                    )
                    .await
            }
        });
        let AgentEvent::PermissionRequest(request) = event_rx.recv_async().await.unwrap().event
        else {
            panic!("{PLAN_PROMPT_MISSING}");
        };
        let accepted = manager.answer(PLAN_REQUEST_ID, answer);
        if !accepted {
            manager.answer(PLAN_REQUEST_ID, PermissionAnswer::Deny);
        }
        (accepted, task.await, request)
    }

    const PLAN_REQUEST_ID: &str = "plan-request";
    const PLAN_PROMPT_MISSING: &str = "plan-scoped call did not raise a prompt";
    const WORKDIR_OPTION: &str = "allow_commands_in_workdir";

    fn workdir_grant(lifetime: PermissionLifetime) -> PermissionAnswer {
        PermissionAnswer::AllowOption {
            option_id: WORKDIR_OPTION.into(),
            lifetime,
        }
    }

    /// The point of plan containment: one approval while planning is enough to
    /// keep exploring, so the model can run the scripts the plan needs.
    #[test]
    fn a_conversation_grant_covers_later_plan_scoped_commands() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);

            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "cargo check > /tmp/out")
                    .await
                    .is_err()
            );

            let (accepted, granted, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                "cargo check",
                workdir_grant(PermissionLifetime::Conversation),
            )
            .await;
            assert!(accepted);
            assert!(granted.is_ok());

            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "python3 explore.py")
                    .await
                    .is_ok()
            );
        });
    }

    /// Containment cuts the other way too: authority the plan never asked for
    /// does not apply to it, however durable that authority is.
    #[test]
    fn a_project_grant_does_not_cover_a_plan_scoped_command() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let command = "cargo check > /tmp/out";

            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                workdir_grant(PermissionLifetime::Project),
            )
            .await
            .unwrap();

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, command)
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, command)
                    .await
                    .is_err()
            );
        });
    }

    #[test_case(PermissionLifetime::Project ; "project")]
    #[test_case(PermissionLifetime::Global ; "global")]
    fn a_plan_scoped_prompt_refuses_a_lifetime_that_outlives_the_plan(
        lifetime: PermissionLifetime,
    ) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);

            let (accepted, _, request) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                "cargo check",
                workdir_grant(lifetime),
            )
            .await;

            assert!(!accepted);
            let offered = request
                .options
                .iter()
                .find(|option| option.id == WORKDIR_OPTION)
                .expect("the workdir authority is offered");
            assert_eq!(
                offered.allowed_lifetimes,
                vec![PermissionLifetime::Conversation]
            );
        });
    }

    /// Containment narrows. A deny is narrowing, so the plan still obeys it
    /// even though the conversation grant would otherwise have covered it.
    #[test]
    fn a_project_deny_outranks_a_plan_scoped_conversation_grant() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let denied = "cargo check > /tmp/out";

            let (denied_accepted, refused, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                denied,
                PermissionAnswer::DenyAlwaysLocal,
            )
            .await;
            assert!(denied_accepted);
            assert!(refused.is_err());
            let (accepted, _, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                "cargo check",
                workdir_grant(PermissionLifetime::Conversation),
            )
            .await;
            assert!(accepted);

            // The grant is live for the workdir, and still cannot reach what
            // the project denied.
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "python3 explore.py")
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, denied)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn broad_shell_authority_silences_opaque_command_prompts() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let opaque = "cargo check > /tmp/out";

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, opaque)
                    .await
                    .is_err()
            );

            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                PermissionAnswer::AllowOption {
                    option_id: "allow_commands_in_workdir".into(),
                    lifetime: PermissionLifetime::Conversation,
                },
            )
            .await
            .unwrap();

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, opaque)
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn configured_command_allows_never_cover_opaque_commands() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager = Arc::new(PermissionManager::new_persistent_in(
                make_config(vec![allow_rule("*")]),
                project.clone(),
                Arc::default(),
                StateDir::from_path(temp.path().join("state")),
            ));

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, "cargo check > /tmp/out")
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn exact_global_rule_matches_after_restart_without_storing_raw_input() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let secret = "cargo test --token super-secret-value";
            let input = serde_json::json!({"command": secret});
            let manager = persistent_manager(state_dir.clone(), &project);

            answer_enforcement(
                Arc::clone(&manager),
                secret,
                input.clone(),
                PermissionAnswer::AllowAlwaysGlobal,
            )
            .await
            .unwrap();

            let state = PermissionState::open(&state_dir).unwrap();
            let serialized = serde_json::to_string(state.records()).unwrap();
            assert!(!serialized.contains(secret));
            assert!(!serialized.contains("super-secret-value"));
            drop(manager);

            let restarted = persistent_manager(state_dir, &project);
            enforce_without_prompt(&restarted, secret, input.clone())
                .await
                .unwrap();
            assert!(
                enforce_without_prompt(
                    &restarted,
                    "cargo test --token changed",
                    serde_json::json!({"command": "cargo test --token changed"}),
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn url_subtree_project_rule_survives_restart_without_storing_url() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let manager = persistent_manager(state_dir.clone(), &project);
            let approved = "https://example.com/docs/page?token=secret";

            answer_tool_enforcement(
                Arc::clone(&manager),
                "webfetch",
                approved,
                serde_json::json!({"url": approved}),
                PermissionAnswer::AllowOption {
                    option_id: "allow_url_subtree".into(),
                    lifetime: PermissionLifetime::Project,
                },
            )
            .await
            .unwrap();
            let state = PermissionState::open(&state_dir).unwrap();
            let stored = serde_json::to_string(state.records()).unwrap();
            assert!(!stored.contains("example.com"));
            assert!(!stored.contains("secret"));
            drop(manager);

            let restarted = persistent_manager(state_dir, &project);
            let descendant = "https://example.com/docs/page/child?other=value";
            enforce_tool_without_prompt(
                &restarted,
                "webfetch",
                descendant,
                serde_json::json!({"url": descendant, "timeout": 10}),
            )
            .await
            .unwrap();
            assert!(
                enforce_tool_without_prompt(
                    &restarted,
                    "webfetch",
                    "https://example.com/docs/sibling",
                    serde_json::json!({"url": "https://example.com/docs/sibling"}),
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn persistent_deny_is_exact_and_wins_over_conversation_allow() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let input = serde_json::json!({"command": "cargo publish"});
            let manager = persistent_manager(state_dir.clone(), &project);

            answer_enforcement(
                Arc::clone(&manager),
                "cargo publish",
                input.clone(),
                PermissionAnswer::DenyAlwaysGlobal,
            )
            .await
            .unwrap_err();

            let request = PermissionRequest::from_legacy(
                "conversation".into(),
                ToolKey::native("bash"),
                vec!["cargo publish".into()],
                input.clone(),
                &project,
                false,
            );
            let allow = PermissionRuleRecord::conversation(
                request
                    .option_rule("allow_exact", PermissionLifetime::Conversation)
                    .unwrap(),
            )
            .unwrap();
            manager.load_structured_conversation_rules(vec![allow]);

            assert!(
                enforce_without_prompt(&manager, "cargo publish", input)
                    .await
                    .is_err()
            );
            let inventory = manager.structured_rule_inventory().unwrap();
            assert!(inventory.iter().any(|record| {
                record.rule.effect == StructuredPermissionEffect::Deny
                    && record.rule.lifetime == PermissionLifetime::Global
            }));
        });
    }

    #[test]
    fn revocation_is_durable_and_visible_to_live_managers() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let input = serde_json::json!({"command": "cargo check"});
            let first = persistent_manager(state_dir.clone(), &project);
            let second = persistent_manager(state_dir.clone(), &project);
            answer_enforcement(
                Arc::clone(&first),
                "cargo check",
                input.clone(),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();
            let id = second.structured_rule_inventory().unwrap()[0].id.clone();

            assert_eq!(
                second.revoke_structured_rule(&id).unwrap(),
                Some(RevokedRuleScope::Project)
            );
            assert!(first.structured_rule_inventory().unwrap().is_empty());
            drop(first);
            drop(second);

            let restarted = persistent_manager(state_dir, &project);
            assert!(restarted.structured_rule_inventory().unwrap().is_empty());
            assert!(
                enforce_without_prompt(&restarted, "cargo check", input)
                    .await
                    .is_err()
            );
        });
    }

    /// A second process writes straight to the store, so the manager's cached
    /// records are the only thing that could answer. Both directions have to
    /// reach it without a restart.
    #[test]
    fn live_managers_follow_rules_written_by_another_process() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let input = serde_json::json!({"command": "cargo check"});
            let manager = persistent_manager(state_dir.clone(), &project);
            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                input.clone(),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();
            let granted = manager.structured_rule_inventory().unwrap().remove(0);
            let mut elsewhere = PermissionState::open(&state_dir).unwrap();

            assert!(elsewhere.revoke(&granted.id).unwrap());
            assert!(
                enforce_without_prompt(&manager, "cargo check", input.clone())
                    .await
                    .is_err()
            );

            elsewhere
                .insert(granted.project.clone(), granted.rule.clone())
                .unwrap();
            assert!(
                enforce_without_prompt(&manager, "cargo check", input)
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn project_rules_load_only_for_the_canonical_project() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let first = persistent_manager(state_dir.clone(), &first_project);
            answer_enforcement(
                Arc::clone(&first),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();

            let same_project = persistent_manager(state_dir.clone(), &first_project);
            let other_project = persistent_manager(state_dir, &second_project);
            assert_eq!(same_project.structured_rule_inventory().unwrap().len(), 1);
            assert!(
                other_project
                    .structured_rule_inventory()
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn changing_project_updates_builtin_and_persistent_rules() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let manager = persistent_manager(
                StateDir::from_path(temp.path().join("state")),
                &first_project,
            );
            let first_file = first_project
                .join("file.txt")
                .to_string_lossy()
                .into_owned();
            let second_file = second_project
                .join("file.txt")
                .to_string_lossy()
                .into_owned();

            let write_first = |manager: &PermissionManager| {
                legacy_request(manager, ToolKey::native("write"), &[&first_file])
            };
            let write_second = |manager: &PermissionManager| {
                legacy_request(manager, ToolKey::native("write"), &[&second_file])
            };
            assert!(allows_without_prompt(&manager, &write_first(&manager)));
            assert!(!allows_without_prompt(&manager, &write_second(&manager)));
            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();

            manager.set_project(&second_project);
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
            assert!(!allows_without_prompt(&manager, &write_first(&manager)));
            assert!(allows_without_prompt(&manager, &write_second(&manager)));
            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo check",
                    serde_json::json!({"command": "cargo check"}),
                )
                .await
                .is_err()
            );

            answer_enforcement(
                Arc::clone(&manager),
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();
            manager.set_project(&first_project);
            enforce_without_prompt(
                &manager,
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
            )
            .await
            .unwrap();
            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo test",
                    serde_json::json!({"command": "cargo test"}),
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn corrupt_store_fails_closed_without_overwrite() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let state_path = temp.path().join("state");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(state_path);
            let database = SessionDatabase::open_state(&state_dir).unwrap();
            database
                .global_state_set(PERMISSION_RULES_STATE_KEY, &"corrupt")
                .unwrap();
            let manager = persistent_manager(state_dir, &project);

            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo test",
                    serde_json::json!({"command": "cargo test"}),
                )
                .await
                .is_err()
            );
            assert_eq!(
                database
                    .global_state_get::<String>(PERMISSION_RULES_STATE_KEY)
                    .unwrap(),
                Some("corrupt".into())
            );
        });
    }

    #[test]
    fn persistence_failure_blocks_execution_without_session_fallback() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let state_path = temp.path().join("state");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&state_path).unwrap();
            let manager = persistent_manager(StateDir::from_path(state_path.clone()), &project);
            let scopes = crate::tools::PermissionScopes::single("cargo test".into());
            let input = serde_json::json!({"command": "cargo test"});
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let legacy_rx = Arc::clone(&legacy_rx);
                async move {
                    manager
                        .enforce(
                            &ToolKey::native("bash"),
                            &scopes,
                            &input,
                            &event_tx,
                            Some(&legacy_rx),
                            "failed-persist",
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                }
            });
            let _ = event_rx.recv_async().await.unwrap();
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second-failed-persist",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            second_events.recv_async().await.unwrap();
            SessionDatabase::open_state(&StateDir::from_path(state_path))
                .unwrap()
                .global_state_set(PERMISSION_RULES_STATE_KEY, &"corrupt")
                .unwrap();
            assert!(!manager.answer("failed-persist", PermissionAnswer::AllowAlwaysGlobal));
            assert_eq!(manager.pending_count(), 2);
            assert!(second_events.is_empty());
            assert!(manager.answer("failed-persist", PermissionAnswer::Deny));
            assert!(manager.answer("second-failed-persist", PermissionAnswer::Deny));

            assert!(task.await.is_err());
            assert!(second.await.is_err());
            assert!(manager.structured_conversation_rules_snapshot().is_empty());
        });
    }

    #[test]
    fn fresh_manager_fork_has_clean_conversation_rules() {
        let manager = default_mgr();
        let request = PermissionRequest::from_legacy(
            "conversation".into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/tmp"),
            false,
        );
        let record = PermissionRuleRecord::conversation(
            request
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap(),
        )
        .unwrap();
        manager.load_structured_conversation_rules(vec![record]);

        let fresh = manager.fork();

        assert!(fresh.structured_conversation_rules_snapshot().is_empty());
        assert_eq!(manager.structured_conversation_rules_snapshot().len(), 1);
    }

    fn log_request(resources: Vec<PermissionResource>) -> PermissionRequest {
        let mut request = PermissionRequest::from_legacy(
            "log".into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/tmp"),
            false,
        );
        request.resources = resources;
        request
    }

    fn log_coverage() -> Option<ResourceCoverage> {
        Some(ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: "this command".into(),
        })
    }

    fn log_resource(value: &str, protected: bool, requires_prompt: bool) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::Command,
            value: value.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected,
            requires_prompt,
            attributes: BTreeMap::new(),
        }
    }

    #[test_case(true, true, true, PROMPT_REASON_FORCED; "force_prompt outranks every other cause")]
    #[test_case(false, true, true, PROMPT_REASON_PROTECTED; "protected outranks an ask rule")]
    #[test_case(false, false, true, PROMPT_REASON_ASK_RULE; "ask rule outranks bare uncovered")]
    #[test_case(false, false, false, PROMPT_REASON_UNCOVERED; "uncovered is the fallback")]
    fn prompt_forcing_reason_reports_the_highest_precedence_cause(
        forced: bool,
        protected: bool,
        ask_rule: bool,
        expected: &str,
    ) {
        let request = log_request(vec![log_resource("cargo test", protected, false)]);

        let reason = prompt_forcing_reason(&request, &[None], forced, ask_rule);

        assert_eq!(reason, expected);
    }

    #[test]
    fn prompt_forcing_reason_ignores_covered_resources() {
        let request = log_request(vec![
            log_resource("git status", true, true),
            log_resource("cargo test", false, false),
        ]);

        let reason = prompt_forcing_reason(&request, &[log_coverage(), None], false, false);

        assert_eq!(reason, PROMPT_REASON_UNCOVERED);
    }

    #[test]
    fn prompt_forcing_reason_prefers_protected_over_requires_prompt() {
        let request = log_request(vec![
            log_resource("cargo test", false, true),
            log_resource("git push", true, false),
        ]);

        let reason = prompt_forcing_reason(&request, &[None, None], false, false);

        assert_eq!(reason, PROMPT_REASON_PROTECTED);
    }

    #[test]
    fn uncovered_resource_summary_lists_only_uncovered_resources() {
        let request = log_request(vec![
            log_resource("already granted", false, false),
            log_resource("brand new", false, false),
        ]);

        let summary = uncovered_resource_summary(&request, &[log_coverage(), None]);

        assert_eq!(summary, "Command:brand new");
    }

    #[test]
    fn uncovered_resource_summary_caps_the_resource_count() {
        let resources = (0..PROMPT_LOG_MAX_RESOURCES + 3)
            .map(|index| log_resource(&format!("cmd{index}"), false, false))
            .collect();
        let request = log_request(resources);
        let coverage = vec![None; PROMPT_LOG_MAX_RESOURCES + 3];

        let summary = uncovered_resource_summary(&request, &coverage);

        assert!(summary.ends_with(", ..."), "{summary}");
        assert_eq!(
            summary.matches("Command:").count(),
            PROMPT_LOG_MAX_RESOURCES
        );
    }

    #[test]
    fn uncovered_resource_summary_truncates_a_long_value() {
        let value = "x".repeat(PROMPT_LOG_MAX_VALUE_CHARS * 2);
        let request = log_request(vec![log_resource(&value, false, false)]);

        let summary = uncovered_resource_summary(&request, &[None]);

        assert_eq!(
            summary,
            format!("Command:{}", "x".repeat(PROMPT_LOG_MAX_VALUE_CHARS))
        );
    }

    #[test_case(PermissionAnswer::AllowOnce, "allow", "", "once")]
    #[test_case(PermissionAnswer::AllowSession, "allow_session", "", "conversation")]
    #[test_case(
        PermissionAnswer::AllowAlwaysLocal,
        "allow_always_local",
        "",
        "project"
    )]
    #[test_case(
        PermissionAnswer::AllowAlwaysGlobal,
        "allow_always_global",
        "",
        "global"
    )]
    #[test_case(PermissionAnswer::Deny, "deny", "", "")]
    #[test_case(PermissionAnswer::DenyAlwaysGlobal, "deny_always_global", "", "")]
    fn answer_log_fields_splits_the_answer_into_groupable_fields(
        answer: PermissionAnswer,
        expected_answer: &str,
        expected_option: &str,
        expected_lifetime: &str,
    ) {
        assert_eq!(
            answer_log_fields(&answer),
            (
                expected_answer,
                Cow::Borrowed(expected_option),
                expected_lifetime
            )
        );
    }

    #[test]
    fn answer_log_fields_reports_the_option_and_its_chosen_lifetime() {
        let answer = PermissionAnswer::AllowOption {
            option_id: "allow_subtree".into(),
            lifetime: PermissionLifetime::Project,
        };

        assert_eq!(
            answer_log_fields(&answer),
            ("allow_option", Cow::Borrowed("allow_subtree"), "project")
        );
    }
}
