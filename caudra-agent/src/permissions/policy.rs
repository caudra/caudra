use super::pattern_matching::CompiledPattern;
use super::{
    NORMALIZED_COMMAND_ATTRIBUTE, PermissionArgumentConstraint, PermissionBroker,
    PermissionExecutorKind, PermissionLifetime, PermissionManager, PermissionRequest,
    PermissionResourceAccess, PermissionResourceConstraint, PermissionResourceKind,
    PermissionResourceSelector, PermissionRuleRecord, PermissionSubject, PolicyRule, RuleOrigin,
    SUBTREE_SCOPE_SUFFIX, StructuredPermissionEffect, StructuredPermissionRule, command_pattern,
    hex_encode, normalize_configured_selector, permission_rules_cover_request,
};
use crate::tools::native;
use crate::tools::native::memory::{self, LOCAL_MEMORY_RESOURCE};
use caudra_config::{
    DefaultEffect, Effect, FILE_WRITE_TOOLS, PermissionReviewCandidate, PermissionRule,
    PermissionsConfig, ToolKey,
};
use caudra_storage::StateDir;
use caudra_storage::paths::incremental_canonicalize;
use caudra_storage::permission_config_trust::{
    is_project_trusted as is_permission_config_trusted, is_remote_asset_trusted,
    revoke_project_trust as revoke_project_permission_config, revoke_remote_asset_trust,
    trust_project as trust_project_permission_config, trust_remote_asset,
};
use caudra_storage::permission_state::PermissionState;
use caudra_storage::permission_state::mutation::PermissionOwner;
use caudra_storage::permission_state::validate_command_templates;
use caudra_storage::projects::project_document_dirs;
use caudra_storage::sessions::SESSIONS_DB_FILE;
use caudra_workspace::ProjectAssetTrustKey;
use sha2::Digest;
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, Weak};
use thiserror::Error;
use tracing::warn;

const MAX_LOCAL_POLICY_BYTES: u64 = 4 * 1024 * 1024;
const LOCAL_SOURCE_PATH_CHANGED: &str = "local policy source path differs from verified loading";
const LOCAL_SOURCE_TOO_LARGE: &str = "local policy source exceeds its bound";
const LOCAL_SOURCE_CHANGED: &str = "local policy source changed since verified loading";
const LOCAL_SOURCE_NOT_FILE: &str = "local policy source is not a bounded file";

/// Set by the shell tool on a command that only observes and can only reach
/// inside the project. Nothing else may set it: the builtin rule below reads it
/// as the whole justification for running without asking.
pub const CONFINED_READ_ATTRIBUTE: &str = "confined_read";

pub const CONFINED_READ_VALUE: &str = "true";

pub(super) const SHELL_EXECUTION_CONTRACT: &str = "shell.execution.v1";

pub(super) const WORKCELL_TOOL_OWNER: &str = "workcell";

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

pub(super) const PROJECT_READ_TOOLS: &[&str] = &[
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
pub(super) const TRUSTED_UNSCOPED_TOOLS: &[&str] = &[
    "automation",
    "batch",
    "python_execution",
    "question",
    "skill",
    "task",
    "todo_write",
    "tool_output",
    "workflow",
];

pub(super) fn validate_compiled_templates(
    rule: &StructuredPermissionRule,
) -> Result<(), PermissionPolicyError> {
    validate_command_templates(rule).map_err(|error| PermissionPolicyError(error.to_string()))?;
    for resource in &rule.resources {
        if let PermissionResourceSelector::CommandTemplate { definition } = &resource.selector {
            CompiledPattern::compile(definition)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
        }
    }
    Ok(())
}

/// The scratch directory joins the project as a pre-allowed root. Work that
/// does not belong in the workspace has to go somewhere, and a prompt for every
/// temporary file teaches nothing: the directory is Caudra's own, holds no
/// secret, and is where `TMPDIR` already points. Paths beside it under the
/// shared temp root keep prompting, which is what makes the free one worth
/// aiming at.
///
/// The whole scratch root is covered, not the current project's subdirectory
/// inside it. `/cd` rebinds the project and rebuilds these rules, while
/// `TMPDIR` stays where startup put it, so a narrower grant would start
/// prompting for the very directory the model was told to use.
///
/// Which root that is comes from [`crate::scratch`], because in remote mode the
/// directory the model was handed lives on the host running the tools and the
/// local one names nothing any of them can reach. The rule follows the advertised
/// path in both modes, and covers no more than the root holding it.
///
/// The project's own plans and notes are readable the same way, never writable
/// through these rules: the plan file and the memory tool keep them under the
/// state directory, and asking before reading back what this project wrote
/// protects nothing. Only while tools run on this machine, because a remote
/// host's path of the same spelling is not Caudra's.
pub(super) fn builtin_rules(cwd: &Path, state_dir: Option<&StateDir>) -> Vec<PermissionRule> {
    let allow = |tool: &str, scope: &str| PermissionRule {
        tool: ToolKey::native(tool),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    };
    let cwd_glob = format!(
        "{}/**",
        caudra_storage::paths::canonicalize_clean(cwd).display()
    );
    let roots: Vec<String> = std::iter::once(cwd_glob)
        .chain(crate::scratch::permission_root().map(|scratch| format!("{scratch}/**")))
        .collect();
    let mut rules: Vec<PermissionRule> = Vec::new();
    for root in &roots {
        rules.extend(FILE_WRITE_TOOLS.iter().map(|tool| allow(tool, root)));
        rules.extend(PROJECT_READ_TOOLS.iter().map(|tool| allow(tool, root)));
    }
    let document_roots = state_dir
        .filter(|_| crate::scratch::tools_run_locally())
        .into_iter()
        .flat_map(|state_dir| project_document_dirs(state_dir, cwd))
        .filter_map(|dir| incremental_canonicalize(&dir))
        .map(|dir| format!("{}/**", dir.display()));
    for root in document_roots {
        rules.extend(PROJECT_READ_TOOLS.iter().map(|tool| allow(tool, &root)));
    }
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
///
/// Browsing a remote session's notes is the same read of the project's own
/// notes that a local session's memory rules already allow; only the memory
/// tool's own identity reaches the opaque resource those notes are named by.
pub(super) fn builtin_structured_rules() -> Vec<PolicyRule> {
    let allow = |subject: PermissionSubject, resource: PermissionResourceConstraint| PolicyRule {
        origin: RuleOrigin::Builtin,
        rule: StructuredPermissionRule {
            subject,
            executor: PermissionExecutorKind::Native,
            resources: vec![resource],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        },
    };
    vec![
        allow(
            PermissionSubject::Native {
                owner: WORKCELL_TOOL_OWNER.into(),
                contract: SHELL_EXECUTION_CONTRACT.into(),
            },
            PermissionResourceConstraint {
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
            },
        ),
        allow(
            PermissionSubject::Native {
                owner: native::OWNER.into(),
                contract: memory::permission_contract().into(),
            },
            PermissionResourceConstraint {
                kind: PermissionResourceKind::Custom {
                    name: LOCAL_MEMORY_RESOURCE.into(),
                },
                selector: PermissionResourceSelector::Any,
                access: Some(PermissionResourceAccess::Read),
                protected: Some(false),
                attributes: BTreeMap::new(),
            },
        ),
    ]
}

/// Permission rules declared by Lua plugins via
/// `caudra.api.register_permission_rule`, keyed by plugin name. Shared between
/// the Lua runtime (writer, on plugin load/unload) and every
/// [`PermissionManager`] (reader).
#[derive(Default)]
pub struct PluginRuleStore {
    pub(super) rules: Mutex<HashMap<Arc<str>, Vec<PermissionRule>>>,
    pub(super) brokers: Mutex<Vec<Weak<PermissionBroker>>>,
    pub(super) edit_revision: RwLock<u64>,
    sources: Mutex<HashMap<Arc<str>, VerifiedLocalSourceLocator>>,
}

impl PluginRuleStore {
    pub(super) fn lock(&self) -> MutexGuard<'_, HashMap<Arc<str>, Vec<PermissionRule>>> {
        self.rules.lock().unwrap_or_else(|e| {
            warn!("plugin rule mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    /// An empty `rules` removes the entry, so a reload that registers
    /// nothing clears the stale rules.
    pub fn replace(&self, plugin: &str, rules: Vec<PermissionRule>) {
        self.replace_with_source(plugin, rules, None);
    }

    pub fn replace_with_source(
        &self,
        plugin: &str,
        rules: Vec<PermissionRule>,
        source: Option<VerifiedLocalSourceLocator>,
    ) {
        let mut revision = self
            .edit_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut map = self.lock();
        let mut sources = self
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if rules.is_empty() {
            map.remove(plugin);
            sources.remove(plugin);
        } else {
            map.insert(Arc::from(plugin), rules);
            if let Some(source) = source {
                sources.insert(Arc::from(plugin), source);
            } else {
                sources.remove(plugin);
            }
        }
        *revision += 1;
        drop(sources);
        drop(map);
        self.notify_policy_changed();
    }

    pub fn remove(&self, plugin: &str) {
        self.replace(plugin, Vec::new());
    }

    pub(super) fn observe(&self, broker: &Arc<PermissionBroker>) {
        let mut brokers = self
            .brokers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        brokers.retain(|broker| broker.strong_count() > 0);
        if !brokers
            .iter()
            .filter_map(Weak::upgrade)
            .any(|existing| Arc::ptr_eq(&existing, broker))
        {
            brokers.push(Arc::downgrade(broker));
        }
    }

    pub(super) fn notify_policy_changed(&self) {
        let brokers: Vec<_> = self
            .brokers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for broker in brokers {
            broker.notify_policy_changed("");
        }
    }

    pub fn snapshot(&self) -> Vec<PermissionRule> {
        self.lock().values().flatten().cloned().collect()
    }

    fn source_snapshot(&self) -> Vec<ActivePolicyRule> {
        let _revision = self
            .edit_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let rules = self.lock();
        let sources = self
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        rules
            .iter()
            .flat_map(|(plugin, rules)| {
                let source = sources.get(plugin).cloned();
                rules.iter().cloned().map(move |rule| ActivePolicyRule {
                    origin: RuleOrigin::Plugin,
                    rule,
                    verified_local_source_locator: source.clone(),
                })
            })
            .collect()
    }
}

#[derive(Clone)]
pub(super) struct ConfiguredPolicy {
    pub(super) verified_sources: Vec<(PermissionRule, VerifiedLocalSourceLocator)>,
    pub(super) rules: Vec<PermissionRule>,
    pub(super) project_allow_rules: Vec<PermissionRule>,
    pub(super) project_config_digest: Option<String>,
    pub(super) project_config_root: Option<PathBuf>,
    pub(super) review_candidates: Vec<PermissionReviewCandidate>,
    pub(super) default: DefaultEffect,
    pub(super) tool_defaults: HashMap<ToolKey, DefaultEffect>,
    pub(super) remote_allow_rules: Vec<PermissionRule>,
    pub(super) remote_allow_defaults: HashMap<ToolKey, DefaultEffect>,
    pub(super) remote_default_allow: bool,
    pub(super) remote_restrictive_rules: Vec<PermissionRule>,
    pub(super) remote_restrictive_defaults: HashMap<ToolKey, DefaultEffect>,
    pub(super) remote_restrictive_default: Option<DefaultEffect>,
    pub(super) remote_policy_invalid: bool,
    pub(super) remote_permission_asset: Option<(ProjectAssetTrustKey, String)>,
    remote_snapshot: Option<crate::remote_project_context::RemotePermissionAsset>,
    pub(super) remote_review_candidates: Vec<PermissionReviewCandidate>,
}

impl ConfiguredPolicy {
    pub(super) fn clear_remote(&mut self, fail_closed: bool) {
        self.remote_restrictive_rules.clear();
        self.remote_restrictive_defaults.clear();
        self.remote_restrictive_default = fail_closed.then_some(DefaultEffect::Deny);
        self.remote_allow_rules.clear();
        self.remote_allow_defaults.clear();
        self.remote_default_allow = false;
        self.remote_review_candidates.clear();
        self.remote_permission_asset = None;
        self.remote_snapshot = None;
        self.remote_policy_invalid = fail_closed;
    }
}

pub(super) struct SharedPolicy {
    pub(super) state: Option<PermissionState>,
    pub(super) error: Option<String>,
}

pub(super) struct SharedPermissionState {
    pub(super) state_dir: StateDir,
    pub(super) policy: Mutex<SharedPolicy>,
    pub(super) broker: Arc<PermissionBroker>,
}

/// One line of the configured, builtin, or plugin policy, for display in the
/// permissions picker. These rules are edited at their source rather than
/// revoked, so the picker shows them read-only.
#[derive(Clone)]
pub struct ActivePolicyRule {
    /// `Config`, `Builtin`, or `Plugin`; never a user's own lifetime.
    pub origin: RuleOrigin,
    pub rule: PermissionRule,
    pub verified_local_source_locator: Option<VerifiedLocalSourceLocator>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLocalSourceLocator {
    path: PathBuf,
    content_digest: String,
    plugin_entrypoint: bool,
}

impl VerifiedLocalSourceLocator {
    pub fn from_loaded_file(
        path: &Path,
        content_digest: &str,
    ) -> Result<Self, PermissionPolicyError> {
        if !path.is_absolute() {
            return Err(PermissionPolicyError(
                "local policy source must be absolute".into(),
            ));
        }
        let canonical =
            fs::canonicalize(path).map_err(|error| PermissionPolicyError(error.to_string()))?;
        if canonical.as_os_str() != path.as_os_str() {
            return Err(PermissionPolicyError(LOCAL_SOURCE_PATH_CHANGED.into()));
        }
        let source = Self {
            path: canonical,
            content_digest: content_digest.into(),
            plugin_entrypoint: false,
        };
        source.verify_current()?;
        Ok(source)
    }

    pub fn from_loaded_entrypoint(
        path: &Path,
        bytes: &[u8],
    ) -> Result<Self, PermissionPolicyError> {
        if bytes.len() as u64 > MAX_LOCAL_POLICY_BYTES {
            return Err(PermissionPolicyError(LOCAL_SOURCE_TOO_LARGE.into()));
        }
        let mut source = Self::from_loaded_file(path, &hex_encode(&Sha256::digest(bytes)))?;
        source.plugin_entrypoint = true;
        Ok(source)
    }

    pub fn is_plugin_entrypoint(&self) -> bool {
        self.plugin_entrypoint
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    pub fn verify_loaded_bytes(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), PermissionPolicyError> {
        if path.as_os_str() != self.path.as_os_str() {
            return Err(PermissionPolicyError(LOCAL_SOURCE_PATH_CHANGED.into()));
        }
        if bytes.len() as u64 > MAX_LOCAL_POLICY_BYTES {
            return Err(PermissionPolicyError(LOCAL_SOURCE_TOO_LARGE.into()));
        }
        if hex_encode(&Sha256::digest(bytes)) != self.content_digest {
            return Err(PermissionPolicyError(LOCAL_SOURCE_CHANGED.into()));
        }
        Ok(())
    }

    pub fn verify_current(&self) -> Result<(), PermissionPolicyError> {
        let canonical = fs::canonicalize(&self.path)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        if canonical.as_os_str() != self.path.as_os_str() {
            return Err(PermissionPolicyError(LOCAL_SOURCE_PATH_CHANGED.into()));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options
            .open(&self.path)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        if !metadata.is_file() || metadata.len() > MAX_LOCAL_POLICY_BYTES {
            return Err(PermissionPolicyError(LOCAL_SOURCE_NOT_FILE.into()));
        }
        let mut bytes = Vec::new();
        file.take(MAX_LOCAL_POLICY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        self.verify_loaded_bytes(&canonical, &bytes)
    }
}

#[derive(Debug, Error)]
#[error("structured permission policy unavailable: {0}")]
pub struct PermissionPolicyError(pub(super) String);

pub(super) type SharedPolicies = HashMap<PathBuf, Weak<SharedPermissionState>>;

pub(super) fn shared_policies() -> &'static Mutex<SharedPolicies> {
    static POLICIES: OnceLock<Mutex<SharedPolicies>> = OnceLock::new();
    POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn shared_policy(state_dir: StateDir) -> Arc<SharedPermissionState> {
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
    pub(super) fn state(&mut self) -> Result<&mut PermissionState, PermissionPolicyError> {
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

/// A stable numeric encoding of an effect for the project-config trust digest.
/// The numbers are part of the digest, so changing one re-prompts every trusted
/// project for consent it already gave.
pub(super) fn config_effect_code(effect: Effect) -> u8 {
    match effect {
        Effect::Allow => 0,
        Effect::Ask => 1,
        Effect::Deny => 2,
    }
}

pub(super) fn project_permission_config_digest(
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

pub(super) fn hash_permission_config_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

pub(super) fn configured_policy(
    config: PermissionsConfig,
    project_config_root: Option<PathBuf>,
) -> ConfiguredPolicy {
    let project_config_digest = project_permission_config_digest(
        &config.project_allow_rules,
        &config.project_restrictive_rules,
    );
    let verified_sources = config
        .loaded_sources
        .iter()
        .filter_map(|source| {
            match VerifiedLocalSourceLocator::from_loaded_file(
                source.path(),
                source.content_digest(),
            ) {
                Ok(locator) => Some((source.rule().clone(), locator)),
                Err(error) => {
                    warn!(error = %error, "loaded permission source is not currently navigable");
                    None
                }
            }
        })
        .collect();
    ConfiguredPolicy {
        verified_sources,
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
        remote_snapshot: None,
        remote_review_candidates: Vec::new(),
    }
}

/// The selector a configured scope stands for, read against the kind of the
/// resource it is being weighed against. A configured scope is one opaque
/// string whose meaning has always depended on the tool that owns it, so the
/// kind is what tells `cmd *` (a command pattern) from `dir *` (a prefix).
pub(super) fn configured_selector(
    scope: &str,
    kind: &PermissionResourceKind,
) -> PermissionResourceSelector {
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
pub(super) fn compile_configured_rule(
    rule: &PermissionRule,
    request: &PermissionRequest,
    project: &Path,
) -> Result<Option<StructuredPermissionRule>, PermissionPolicyError> {
    if !command_rule_tool_matches(&rule.tool, &request.tool) {
        return Ok(None);
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
        return Ok(None);
    }
    // A configured allow never reaches a protected resource: the reviewed text
    // of a protected command or path says more than a scope in a file does. A
    // deny carries the opposite duty and is left free to match one, so it keeps
    // an unconstrained `protected`.
    let protected = (effect == StructuredPermissionEffect::Allow).then_some(false);
    Ok(Some(StructuredPermissionRule {
        subject: request.subject.clone(),
        executor: request.executor.clone(),
        resources: configured_constraints(rule.scope.as_deref(), protected, request, project)?,
        arguments: PermissionArgumentConstraint::Unconstrained,
        // Configured authority is granted outside the conversation, so plan
        // containment withholds it exactly as it withholds a stored project rule.
        lifetime: PermissionLifetime::Project,
        effect,
        family: None,
    }))
}

/// One constraint per resource kind the request carries, because a configured
/// rule names no kind of its own.
///
/// An unscoped rule that has to pin `protected` still emits constraints rather
/// than an empty list, since an empty list is an unrestricted rule that no
/// constraint is ever consulted for.
pub(super) fn configured_constraints(
    scope: Option<&str>,
    protected: Option<bool>,
    request: &PermissionRequest,
    project: &Path,
) -> Result<Vec<PermissionResourceConstraint>, PermissionPolicyError> {
    if scope.is_none() && protected.is_none() {
        return Ok(Vec::new());
    }
    let mut kinds: Vec<PermissionResourceKind> = Vec::new();
    for resource in &request.resources {
        if !kinds.contains(&resource.kind) {
            kinds.push(resource.kind.clone());
        }
    }
    kinds
        .into_iter()
        .map(|kind| {
            let mut selector = scope.map_or(PermissionResourceSelector::Any, |scope| {
                configured_selector(scope, &kind)
            });
            normalize_configured_selector(
                &mut selector,
                &kind,
                project,
                caudra_storage::paths::home().as_deref(),
            )?;
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
            Ok([
                Some(PermissionResourceConstraint {
                    selector,
                    kind,
                    access: None,
                    protected,
                    attributes: BTreeMap::new(),
                }),
                normalized,
            ])
        })
        .collect::<Result<Vec<_>, PermissionPolicyError>>()
        .map(|constraints| constraints.into_iter().flatten().flatten().collect())
}

impl PermissionManager {
    pub(super) fn configured(&self) -> RwLockReadGuard<'_, ConfiguredPolicy> {
        self.configured.read().unwrap_or_else(|error| {
            warn!("permission config lock was poisoned, recovering");
            error.into_inner()
        })
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
        let mut revision = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut current = self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if !current.remote_policy_invalid && current.remote_snapshot.as_ref() == asset {
            before_install()?;
            return Ok(());
        }
        let mut configured = current.clone();
        configured.clear_remote(asset.is_some());
        let Some(asset) = asset else {
            before_install()?;
            *current = configured;
            *revision += 1;
            drop(current);
            self.notify_policy_changed("");
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
        configured.remote_snapshot = Some(asset.clone());
        before_install()?;
        *current = configured;
        *revision += 1;
        drop(current);
        self.notify_policy_changed("");
        Ok(())
    }

    pub fn invalidate_remote_permission_asset(&self) {
        let mut revision = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *revision += 1;
        let mut configured = self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner());
        configured.clear_remote(true);
        drop(configured);
        self.notify_policy_changed("");
    }

    pub(super) fn remote_restrictive_default(&self, tool: &ToolKey) -> Option<DefaultEffect> {
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

    pub(super) fn remote_default_denies(
        &self,
        tool: &ToolKey,
        request: &PermissionRequest,
    ) -> bool {
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
                    .filter_map(|rule| {
                        compile_configured_rule(rule, request, &self.project_cwd())
                            .ok()
                            .flatten()
                    })
                    .collect::<Vec<_>>(),
                request,
            )
        }
    }

    pub(super) fn default_effect(&self, tool: &ToolKey) -> DefaultEffect {
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

    pub(super) fn project_allows_active(&self) -> bool {
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

    pub(super) fn remote_allows_active(&self, configured: &ConfiguredPolicy) -> bool {
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

    pub(super) fn active_config_rules(&self) -> Vec<PermissionRule> {
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
            trust_remote_asset(&policy.state_dir, asset, digest)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            drop(configured);
            self.notify_policy_changed("");
            return Ok(());
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
        drop(project);
        self.notify_policy_changed("");
        Ok(())
    }

    pub fn revoke_project_permission_config_trust(&self) -> Result<(), PermissionPolicyError> {
        if let Some((asset, _)) = &self.configured().remote_permission_asset {
            let policy = self
                .policy
                .as_ref()
                .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
            revoke_remote_asset_trust(&policy.state_dir, asset)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            self.notify_policy_changed("");
            return Ok(());
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
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        drop(project);
        self.notify_policy_changed("");
        Ok(())
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
        let _context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let builtin_rules = self.project().builtin_rules.clone();
        let configured = self.configured().clone();
        let sources = configured.verified_sources;
        let mut entries: Vec<_> = self
            .active_config_rules()
            .into_iter()
            .map(|rule| ActivePolicyRule {
                origin: RuleOrigin::Config,
                verified_local_source_locator: sources
                    .iter()
                    .filter(|(loaded, _)| {
                        loaded == &rule
                            && !configured.remote_allow_rules.contains(&rule)
                            && !configured.remote_restrictive_rules.contains(&rule)
                    })
                    .map(|(_, source)| source)
                    .try_fold(None, |previous, source| match previous {
                        Some(previous) if previous != source => Err(()),
                        _ => Ok(Some(source)),
                    })
                    .ok()
                    .flatten()
                    .cloned(),
                rule,
            })
            .collect();
        entries.extend(builtin_rules.iter().cloned().map(|rule| ActivePolicyRule {
            origin: RuleOrigin::Builtin,
            rule,
            verified_local_source_locator: None,
        }));
        entries.extend(self.plugin_rules.source_snapshot());
        entries
    }

    pub fn set_verified_configuration_sources(
        &self,
        sources: Vec<(PermissionRule, VerifiedLocalSourceLocator)>,
    ) -> Result<(), PermissionPolicyError> {
        let mut context = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        for (_, source) in &sources {
            source.verify_current()?;
        }
        let mut configured = self
            .configured
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if sources.iter().any(|(rule, _)| {
            !configured.rules.contains(rule) && !configured.project_allow_rules.contains(rule)
        }) {
            return Err(PermissionPolicyError(
                "source locator does not identify current local configuration policy".into(),
            ));
        }
        configured.verified_sources = sources;
        *context += 1;
        self.notify_policy_changed("");
        Ok(())
    }

    /// The configured, builtin, and plugin policy compiled against this
    /// request, so one evaluator weighs it alongside the stored rules.
    pub(super) fn configured_structured_rules(
        &self,
        request: &PermissionRequest,
        include_builtin_allows: bool,
    ) -> Result<Vec<PolicyRule>, PermissionPolicyError> {
        let project = self.project_cwd();
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
            .map(|(origin, rule)| {
                compile_configured_rule(rule, request, &project)
                    .map(|rule| rule.map(|rule| PolicyRule { origin, rule }))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|rules| rules.into_iter().flatten().collect())
    }

    /// The rules that may speak to a call, contained to the plan when one is
    /// being built: an authority granted before the plan is not one the plan
    /// asked for, so no persistent allow applies. Configured policy carries a
    /// `Project` lifetime for exactly this reason. Denials and asks are left
    /// alone, because containment narrows and must never widen.
    pub(super) fn applicable_rules_within(
        &self,
        request: &PermissionRequest,
        plan_scoped: bool,
        include_builtin_allows: bool,
    ) -> Result<Vec<PolicyRule>, PermissionPolicyError> {
        let mut rules = self.applicable_structured_rules()?;
        rules.extend(self.configured_structured_rules(request, include_builtin_allows)?);
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

    pub(super) fn applicable_structured_rules(
        &self,
    ) -> Result<Vec<PolicyRule>, PermissionPolicyError> {
        let mut rules = builtin_structured_rules();
        rules.extend(
            self.stored_permission_records()?
                .into_iter()
                .map(|record| PolicyRule {
                    origin: match record.rule.lifetime {
                        PermissionLifetime::Conversation => RuleOrigin::Conversation,
                        PermissionLifetime::Global => RuleOrigin::Global,
                        _ => RuleOrigin::Project,
                    },
                    rule: record.rule,
                }),
        );
        Ok(rules)
    }

    pub(super) fn stored_permission_records(
        &self,
    ) -> Result<Vec<PermissionRuleRecord>, PermissionPolicyError> {
        let snapshots = self
            .durable_permission_snapshots()
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        let mut records = if let Some(snapshot) = snapshots
            .iter()
            .find(|snapshot| matches!(snapshot.revision.owner, PermissionOwner::Conversation(_)))
        {
            self.publish_conversation_snapshot(snapshot.clone())
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            snapshot.records.clone()
        } else {
            self.structured_conversation_rules().clone()
        };
        self.ensure_conversation_policy_valid()?;
        records.retain(PermissionRuleRecord::is_active);
        let project_context = self.project();
        if self.policy.is_none() {
            if let Some(error) = &project_context.policy_context_error {
                return Err(PermissionPolicyError(error.clone()));
            }
            return Ok(records);
        }
        let project = project_context.canonical_project.clone().ok_or_else(|| {
            PermissionPolicyError(
                project_context
                    .policy_context_error
                    .clone()
                    .unwrap_or_else(|| "canonical project is unavailable".into()),
            )
        })?;
        drop(project_context);
        records.extend(
            snapshots
                .into_iter()
                .filter(|snapshot| snapshot.revision.owner == PermissionOwner::Persistent)
                .flat_map(|snapshot| snapshot.records)
                .filter(|record| {
                    record.is_active()
                        && match record.rule.lifetime {
                            PermissionLifetime::Global => record.project.is_none(),
                            PermissionLifetime::Project => {
                                record.project.as_ref() == Some(&project)
                            }
                            PermissionLifetime::Once | PermissionLifetime::Conversation => false,
                        }
                }),
        );
        records
            .into_iter()
            .map(|record| {
                validate_compiled_templates(&record.rule)?;
                Ok(record)
            })
            .collect()
    }
}

pub(super) fn matches_rule(rule_key: &ToolKey, actual: &ToolKey) -> bool {
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

pub(super) fn is_shell_tool(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(name) if matches!(name.as_ref(), "bash" | "shell"))
}

pub(super) fn command_rule_tool_matches(rule: &ToolKey, actual: &ToolKey) -> bool {
    matches!(rule, ToolKey::Wildcard)
        || is_shell_tool(rule) && is_shell_tool(actual)
        || matches_rule(rule, actual)
}

pub(super) fn is_bound_shell_request(request: &PermissionRequest) -> bool {
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

#[cfg(test)]
mod tests {

    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use caudra_storage::sessions::SessionDatabase;
    use caudra_workspace::{AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        LOCAL_SOURCE_CHANGED, LOCAL_SOURCE_NOT_FILE, LOCAL_SOURCE_PATH_CHANGED,
        LOCAL_SOURCE_TOO_LARGE, MAX_LOCAL_POLICY_BYTES, VerifiedLocalSourceLocator,
    };

    use crate::AgentEvent;
    use crate::permissions::tests::{
        CARGO_TEST_COMMAND, COVERAGE_COMMAND, PERMISSION_RULES_STATE_KEY, PLUGIN_EDIT_PATH,
        SHELL_WORKDIR, allow_rule, allowed_by_default, allows_without_prompt, answer_enforcement,
        conversation_grant, coverage_of, coverage_with, covered_flags, decisions,
        denied_by_default, denied_by_rule, deny_rule, enforce_opaque_command_without_prompt,
        enforce_shell_without_prompt, enforce_without_prompt, legacy_request, make_config,
        mgr_with, pending_tool_enforcement, persistent_manager, plugin_edit_rule,
        remote_permission_asset, shell_intent, shell_policy_rule, shell_request, stored_policy,
        workcell_shell_subject,
    };
    use crate::permissions::{
        PermissionAnswer, PermissionExecutorKind, PermissionLifetime, PermissionManager,
        PermissionRequest, PermissionResource, PermissionResourceAccess, PermissionResourceKind,
        PermissionRisk, PermissionRuleRecord, PermissionSubject, PluginRuleStore, RuleOrigin,
        StructuredPermissionDecision, StructuredPermissionEffect, hex_encode,
        permission_rule_intersects_request,
    };
    use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    #[cfg(unix)]
    use caudra_storage::projects::project_document_dirs;
    use std::collections::HashMap;
    use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};
    use std::sync::Arc;

    const LOCAL_SOURCE_FILE: &str = "permissions.toml";
    const LOCAL_SOURCE_DIR: &str = "loaded";
    const LOCAL_SOURCE_BYTES: &[u8] = b"[shell]\ndeny = ['git push *']\n";
    const REPLACEMENT_SOURCE_BYTES: &[u8] = b"[shell]\nallow = ['*']\n";
    #[cfg(unix)]
    const PROJECT_DOCUMENT: &str = "notes.md";
    #[cfg(unix)]
    const DOCUMENT_ESCAPE_LINK: &str = "escape";
    #[cfg(unix)]
    const READ_TOOL: &str = "read";
    #[cfg(unix)]
    const WRITE_TOOL: &str = "write";
    const REMOTE_SCRATCH_ROOT: &str = "/var/folders/xy/caudra";
    const REMOTE_SCRATCH_PROJECT: &str = "/var/folders/xy/caudra/happy-cute-tick";

    fn local_source() -> (TempDir, VerifiedLocalSourceLocator) {
        let directory = TempDir::new().unwrap();
        let parent = directory
            .path()
            .canonicalize()
            .unwrap()
            .join(LOCAL_SOURCE_DIR);
        fs::create_dir(&parent).unwrap();
        let path = parent.join(LOCAL_SOURCE_FILE);
        fs::write(&path, LOCAL_SOURCE_BYTES).unwrap();
        let locator = VerifiedLocalSourceLocator::from_loaded_file(
            &path,
            &hex_encode(&Sha256::digest(LOCAL_SOURCE_BYTES)),
        )
        .unwrap();
        (directory, locator)
    }

    #[test_case(b""; "empty_buffer")]
    #[test_case(REPLACEMENT_SOURCE_BYTES; "different_policy")]
    fn supplied_source_bytes_are_checked_even_when_disk_still_matches(bytes: &[u8]) {
        let (_directory, locator) = local_source();
        locator.verify_current().unwrap();
        assert_eq!(
            locator
                .verify_loaded_bytes(locator.path(), bytes)
                .unwrap_err()
                .0,
            LOCAL_SOURCE_CHANGED,
        );
    }

    #[test_case(false; "disk_changed")]
    #[test_case(true; "disk_deleted")]
    fn supplied_source_verification_does_not_reopen_the_file(deleted: bool) {
        let (_directory, locator) = local_source();
        if deleted {
            fs::remove_file(locator.path()).unwrap();
        } else {
            fs::write(locator.path(), REPLACEMENT_SOURCE_BYTES).unwrap();
        }
        locator
            .verify_loaded_bytes(locator.path(), LOCAL_SOURCE_BYTES)
            .unwrap();
        assert!(locator.verify_current().is_err());
    }

    #[test_case("other.toml", true; "different_file")]
    #[test_case("./permissions.toml", true; "dot_alias")]
    #[test_case("nested/../permissions.toml", true; "parent_alias")]
    #[test_case("permissions.toml", false; "relative_path")]
    fn supplied_source_path_must_identify_the_verified_origin(suffix: &str, absolute: bool) {
        let (_directory, locator) = local_source();
        let path = if absolute {
            let mut raw_path = locator.path().parent().unwrap().as_os_str().to_os_string();
            raw_path.push(MAIN_SEPARATOR_STR);
            raw_path.push(suffix);
            PathBuf::from(raw_path)
        } else {
            PathBuf::from(suffix)
        };
        assert_eq!(
            locator
                .verify_loaded_bytes(&path, LOCAL_SOURCE_BYTES)
                .unwrap_err()
                .0,
            LOCAL_SOURCE_PATH_CHANGED,
        );
    }

    #[test_case(false; "at_bound")]
    #[test_case(true; "over_bound_even_with_matching_digest")]
    fn supplied_source_bound_precedes_digest_acceptance(oversized: bool) {
        let bytes = vec![b'x'; MAX_LOCAL_POLICY_BYTES as usize + usize::from(oversized)];
        let (_directory, mut locator) = local_source();
        locator.content_digest = hex_encode(&Sha256::digest(&bytes));
        let result = locator.verify_loaded_bytes(locator.path(), &bytes);
        if oversized {
            assert_eq!(result.unwrap_err().0, LOCAL_SOURCE_TOO_LARGE);
        } else {
            result.unwrap();
        }
    }

    #[test_case(false; "directory_replacement")]
    #[test_case(true; "oversized_replacement")]
    fn current_source_must_remain_a_bounded_regular_file(oversized: bool) {
        let (_directory, locator) = local_source();
        if oversized {
            fs::write(
                locator.path(),
                vec![b'x'; MAX_LOCAL_POLICY_BYTES as usize + 1],
            )
            .unwrap();
        } else {
            fs::remove_file(locator.path()).unwrap();
            fs::create_dir(locator.path()).unwrap();
        }
        let error = locator.verify_current().unwrap_err();
        if oversized {
            assert_eq!(error.0, LOCAL_SOURCE_NOT_FILE);
        }
    }

    #[cfg(unix)]
    #[test_case(false; "file_symlink")]
    #[test_case(true; "parent_symlink")]
    fn source_provenance_cannot_retarget_identical_bytes(parent: bool) {
        let (directory, locator) = local_source();
        let replacement = directory.path().canonicalize().unwrap().join("replacement");
        fs::create_dir(&replacement).unwrap();
        fs::write(replacement.join(LOCAL_SOURCE_FILE), LOCAL_SOURCE_BYTES).unwrap();
        if parent {
            fs::remove_file(locator.path()).unwrap();
            fs::remove_dir(locator.path().parent().unwrap()).unwrap();
            symlink(&replacement, locator.path().parent().unwrap()).unwrap();
        } else {
            fs::remove_file(locator.path()).unwrap();
            symlink(replacement.join(LOCAL_SOURCE_FILE), locator.path()).unwrap();
        }
        assert_eq!(
            locator.verify_current().unwrap_err().0,
            LOCAL_SOURCE_PATH_CHANGED
        );
        assert_eq!(
            VerifiedLocalSourceLocator::from_loaded_file(locator.path(), locator.content_digest())
                .unwrap_err()
                .0,
            LOCAL_SOURCE_PATH_CHANGED
        );
        assert_eq!(
            VerifiedLocalSourceLocator::from_loaded_entrypoint(locator.path(), LOCAL_SOURCE_BYTES)
                .unwrap_err()
                .0,
            LOCAL_SOURCE_PATH_CHANGED
        );
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

    #[test_case(Effect::Deny; "deny")]
    #[test_case(Effect::Ask; "ask")]
    fn configured_rules_override_the_builtin_echo_allow(effect: Effect) {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("echo *", effect)]),
            PathBuf::from("/tmp"),
        );
        let request = shell_request(&["echo hi"], workcell_shell_subject());
        let rules = manager.configured_structured_rules(&request, true).unwrap();

        match effect {
            Effect::Deny => assert!(
                rules
                    .iter()
                    .any(|policy| permission_rule_intersects_request(&policy.rule, &request))
            ),
            Effect::Ask => {
                let coverage = manager.request_coverage(&request, &rules, true);
                assert!(coverage.must_prompt);
            }
            Effect::Allow => unreachable!(),
        }
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
            let revision = *manager.context_revision.read().unwrap();
            let mut installed = false;
            manager
                .replace_remote_permission_asset_after(Some(&first), || {
                    installed = true;
                    Ok(())
                })
                .unwrap();
            assert!(installed);
            assert_eq!(*manager.context_revision.read().unwrap(), revision);
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
            assert!(*manager.context_revision.read().unwrap() > revision);
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

    /// Asserted on the rule list rather than through an enforcement decision,
    /// so a failure names the missing rule instead of a prompt that appeared
    /// for one of several possible reasons.
    #[test]
    fn builtin_rules_allow_the_scratch_root_and_not_the_temp_root_holding_it() {
        let _scratch_mode = crate::scratch::ScratchGuard::local();
        let project = tempfile::tempdir().unwrap();
        let scratch = caudra_storage::paths::scratch_root().unwrap();
        let scopes = allow_scopes(project.path());

        let glob = |root: &Path| format!("{}/**", root.display());
        assert!(scopes.contains(&glob(&scratch)));
        assert!(!scopes.contains(&glob(&std::env::temp_dir())));
    }

    fn allow_scopes(cwd: &Path) -> Vec<String> {
        super::builtin_rules(cwd, None)
            .into_iter()
            .filter(|rule| rule.effect == Effect::Allow)
            .filter_map(|rule| rule.scope)
            .collect()
    }

    /// In remote mode the pre-allowed root is the one on the host the tools run
    /// on, which is also the one the model was handed. The local scratch root
    /// stops being covered because no remote tool can reach it, and the remote
    /// temp root holding the grant is not covered either: the grant is the
    /// namespace Caudra made, never everything beside it.
    #[test]
    fn builtin_rules_follow_the_remote_scratch_root_when_tools_run_remotely() {
        const REMOTE_TEMP: &str = "/var/folders/xy";
        const REMOTE_IS_COVERED: &str = "the advertised remote directory must be pre-allowed";
        const LOCAL_IS_NOT: &str = "a local scratch root no remote tool can reach must not be";
        const TEMP_IS_NOT: &str = "the remote temp root beside the grant must not be";

        let project = tempfile::tempdir().unwrap();
        let local = caudra_storage::paths::scratch_root().unwrap();
        let _scratch_mode = crate::scratch::ScratchGuard::remote(Some((
            REMOTE_SCRATCH_ROOT,
            REMOTE_SCRATCH_PROJECT,
        )));

        let scopes = allow_scopes(project.path());

        assert!(
            scopes.contains(&format!("{REMOTE_SCRATCH_ROOT}/**")),
            "{REMOTE_IS_COVERED}"
        );
        assert!(
            !scopes.contains(&format!("{}/**", local.display())),
            "{LOCAL_IS_NOT}"
        );
        assert!(
            !scopes.contains(&format!("{REMOTE_TEMP}/**")),
            "{TEMP_IS_NOT}"
        );
    }

    /// A remote host that would not make the directory leaves the project rule
    /// and nothing else. Granting the local root here would be authority over a
    /// machine the model is not working on.
    #[test]
    fn builtin_rules_grant_no_scratch_root_when_the_remote_host_made_none() {
        const ONLY_THE_PROJECT: &str = "an uncreated scratch directory must earn no grant";

        let project = tempfile::tempdir().unwrap();
        let local = caudra_storage::paths::scratch_root().unwrap();
        let _scratch_mode = crate::scratch::ScratchGuard::remote(None);

        let scopes = allow_scopes(project.path());

        assert!(
            !scopes.contains(&format!("{}/**", local.display())),
            "{ONLY_THE_PROJECT}"
        );
        assert!(scopes.contains(&format!(
            "{}/**",
            caudra_storage::paths::canonicalize_clean(project.path()).display()
        )));
    }

    #[cfg(unix)]
    struct ProjectDocuments {
        _temp: TempDir,
        manager: Arc<PermissionManager>,
        plans: PathBuf,
        memories: PathBuf,
        other_project_plans: PathBuf,
    }

    /// Built after the scratch mode is set, as a session's manager is: the rule
    /// is decided when the manager is.
    #[cfg(unix)]
    fn project_documents() -> ProjectDocuments {
        let temp = TempDir::new().unwrap();
        let [project, other_project, outside] =
            ["project", "other", "outside"].map(|name| temp.path().join(name));
        for dir in [&project, &other_project, &outside] {
            fs::create_dir(dir).unwrap();
        }
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let [plans, memories] = project_document_dirs(&state_dir, &project);
        let [other_project_plans, _] = project_document_dirs(&state_dir, &other_project);
        for dir in [&plans, &memories, &other_project_plans] {
            fs::create_dir_all(dir).unwrap();
        }
        symlink(&outside, memories.join(DOCUMENT_ESCAPE_LINK)).unwrap();
        ProjectDocuments {
            manager: persistent_manager(state_dir, &project),
            _temp: temp,
            plans,
            memories,
            other_project_plans,
        }
    }

    #[cfg(unix)]
    fn allowed_without_prompt(documents: &ProjectDocuments, tool: &str, path: &Path) -> bool {
        let manager = &documents.manager;
        allows_without_prompt(
            manager,
            &legacy_request(manager, ToolKey::native(tool), &[&path.to_string_lossy()]),
        )
    }

    #[cfg(unix)]
    #[test_case(READ_TOOL, |documents: &ProjectDocuments| documents.plans.join(PROJECT_DOCUMENT) => true ; "a_plan_reads")]
    #[test_case(READ_TOOL, |documents: &ProjectDocuments| documents.memories.join(PROJECT_DOCUMENT) => true ; "a_note_reads")]
    #[test_case(WRITE_TOOL, |documents: &ProjectDocuments| documents.plans.join(PROJECT_DOCUMENT) => false ; "a_plan_write_asks")]
    #[test_case(READ_TOOL, |documents: &ProjectDocuments| documents.other_project_plans.join(PROJECT_DOCUMENT) => false ; "another_projects_plan_asks")]
    #[test_case(READ_TOOL, |documents: &ProjectDocuments| documents.memories.join(DOCUMENT_ESCAPE_LINK).join(PROJECT_DOCUMENT) => false ; "a_link_out_of_the_notes_asks")]
    fn project_plans_and_memories_are_readable_without_a_prompt(
        tool: &str,
        target: fn(&ProjectDocuments) -> PathBuf,
    ) -> bool {
        let _scratch_mode = crate::scratch::ScratchGuard::local();
        let documents = project_documents();
        allowed_without_prompt(&documents, tool, &target(&documents))
    }

    /// A remote host's tools never read this machine's state directory, and a
    /// path of the same spelling there is not Caudra's to hand out.
    #[cfg(unix)]
    #[test]
    fn project_plans_ask_when_tools_run_remotely() {
        let _scratch_mode = crate::scratch::ScratchGuard::remote(Some((
            REMOTE_SCRATCH_ROOT,
            REMOTE_SCRATCH_PROJECT,
        )));
        let documents = project_documents();

        assert!(!allowed_without_prompt(
            &documents,
            READ_TOOL,
            &documents.plans.join(PROJECT_DOCUMENT)
        ));
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
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            let _ = manager.answer("failed-persist", PermissionAnswer::Deny);
            assert!(manager.answer("second-failed-persist", PermissionAnswer::Deny));

            assert!(task.await.is_err());
            assert!(second.await.is_err());
            assert!(manager.structured_conversation_rules_snapshot().is_empty());
        });
    }
}
