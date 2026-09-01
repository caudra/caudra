use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use caudra_config::{
    DefaultEffect, Effect, FILE_WRITE_TOOLS, PermissionReviewCandidate, PermissionRule,
    PermissionsConfig, ToolKey,
};
use caudra_storage::permission_state::{
    PERMISSION_STATE_FILE, PermissionState, validate_conversation_record,
};
use caudra_storage::{StateDir, now_epoch};
use thiserror::Error;
use tracing::{info, warn};

use crate::{AgentEvent, EventSender};

mod structured;
pub use structured::*;

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

/// Values for the `source` attribute on `caudra.tool_decision` events.
pub const DECISION_SOURCE_RULE: &str = "rule";
pub const DECISION_SOURCE_YOLO: &str = "yolo";
pub const DECISION_SOURCE_USER_ONCE: &str = "user_once";
pub const DECISION_SOURCE_USER_SESSION: &str = "user_session";
pub const DECISION_SOURCE_USER_ALWAYS: &str = "user_always";
pub const DECISION_SOURCE_USER_ABORT: &str = "user_abort";
const BASH_WORKDIR_SCOPE_MARKER: &str = " # caudra-workdir[";
const BASH_WORKDIR_FRAME_MARKER: &str = " # caudra-frame[";
static NEXT_PERMISSION_MANAGER_ID: AtomicU64 = AtomicU64::new(1);
const PROJECT_READ_TOOLS: &[&str] = &[
    "file_glob",
    "file_grep",
    "file_read",
    "glob",
    "grep",
    "index",
    "list",
    "read",
    "view_image",
];
const TRUSTED_UNSCOPED_TOOLS: &[&str] = &[
    "batch",
    "code_execution",
    "question",
    "task",
    "todo_write",
    "tool_output_grep",
    "tool_output_read",
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

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

#[derive(Debug)]
pub enum PermissionCheck {
    Allowed,
    Denied,
    NeedsPrompt {
        tool: ToolKey,
        scopes: Vec<String>,
        force_prompt: bool,
    },
}

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
            } => DECISION_SOURCE_USER_ONCE,
            Self::AllowSession
            | Self::AllowOption {
                lifetime: PermissionLifetime::Conversation,
                ..
            } => DECISION_SOURCE_USER_SESSION,
            Self::AllowAlwaysLocal
            | Self::AllowAlwaysGlobal
            | Self::DenyAlwaysLocal
            | Self::DenyAlwaysGlobal
            | Self::AllowOption { .. } => DECISION_SOURCE_USER_ALWAYS,
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
    session_rules: Mutex<Vec<PermissionRule>>,
    inactive_session_allows: Mutex<Vec<PermissionRule>>,
    structured_conversation_rules: Mutex<Vec<PermissionRuleRecord>>,
    conversation_policy_error: Mutex<Option<String>>,
    broker: Arc<PermissionBroker>,
    config_rules: Vec<PermissionRule>,
    review_candidates: Vec<PermissionReviewCandidate>,
    yolo: AtomicBool,
    /// Whether the user set yolo for this session themselves, which is what
    /// makes it worth persisting.
    yolo_explicit: AtomicBool,
    /// What `--yolo` / `always_yolo` seeded `yolo` with, so a session with no
    /// stored intent falls back to the flag instead of to off.
    seed_yolo: bool,
    default: DefaultEffect,
    tool_defaults: HashMap<ToolKey, DefaultEffect>,
    project: Mutex<ProjectContext>,
    policy: Option<Arc<SharedPermissionState>>,
    plugin_rules: Arc<PluginRuleStore>,
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
    policy: Mutex<SharedPolicy>,
    broker: Arc<PermissionBroker>,
}

struct PendingPermission {
    request: PermissionRequest,
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

#[derive(Clone)]
pub struct EffectivePermissionRule {
    pub source: &'static str,
    pub rule: PermissionRule,
    pub removable: bool,
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
    let key = caudra_storage::paths::normalize_path(&state_dir.path().join(PERMISSION_STATE_FILE));
    let mut policies = shared_policies().lock().unwrap_or_else(|error| {
        warn!("permission policy registry mutex was poisoned, recovering");
        error.into_inner()
    });
    if let Some(policy) = policies.get(&key).and_then(Weak::upgrade) {
        return policy;
    }
    policies.retain(|_, policy| policy.strong_count() > 0);
    let policy = Arc::new(SharedPermissionState {
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
        let config_rules = config.rules;
        let review_candidates = config.review_candidates;
        let builtin_rules = builtin_rules(&cwd);

        // Warn if wildcard deny is present — it blocks ALL tools including builtins.
        let has_wildcard_deny = config_rules
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
        let has_wildcard_allow = config_rules
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
            session_rules: Mutex::new(Vec::new()),
            inactive_session_allows: Mutex::new(Vec::new()),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            broker: policy
                .as_ref()
                .map(|state| Arc::clone(&state.broker))
                .unwrap_or_default(),
            config_rules,
            review_candidates,
            yolo: AtomicBool::new(config.yolo),
            yolo_explicit: AtomicBool::new(false),
            seed_yolo: config.yolo,
            default: config.default,
            tool_defaults: config.tool_defaults,
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
            canonical_project,
            policy_context_error,
            builtin_rules: builtin_rules(cwd),
        };
    }

    /// Fresh manager for a new session runtime: shares config and builtin
    /// rules plus the current yolo state, but owns empty session rules so
    /// restoring one session never clobbers another's grants.
    pub fn fork(&self) -> Self {
        let project = self.project().clone();
        Self {
            id: NEXT_PERMISSION_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
            session_rules: Mutex::new(Vec::new()),
            inactive_session_allows: Mutex::new(Vec::new()),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            broker: Arc::clone(&self.broker),
            config_rules: self.config_rules.clone(),
            review_candidates: self.review_candidates.clone(),
            yolo: AtomicBool::new(self.is_yolo()),
            yolo_explicit: AtomicBool::new(self.yolo_explicit.load(Ordering::Relaxed)),
            seed_yolo: self.seed_yolo,
            default: self.default,
            tool_defaults: self.tool_defaults.clone(),
            project: Mutex::new(project),
            policy: self.policy.clone(),
            plugin_rules: Arc::clone(&self.plugin_rules),
        }
    }

    fn session_rules(&self) -> std::sync::MutexGuard<'_, Vec<PermissionRule>> {
        self.session_rules.lock().unwrap_or_else(|e| {
            warn!("permission mutex was poisoned, recovering");
            e.into_inner()
        })
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
        let reusable_rule =
            match self.commit_structured_decision(&request, &answer, project.as_deref()) {
                Ok(rule) => rule,
                Err(error) => {
                    warn!(%error, request_id, "permission decision was not committed");
                    return false;
                }
            };
        let Some(answered) = remove_pending(&mut pending, self.id, request_id) else {
            return false;
        };
        let mut covered = Vec::new();
        if let Some(rule) = reusable_rule {
            let mut matches = Vec::new();
            for (&manager_id, requests) in pending.iter() {
                for (candidate_id, candidate) in requests {
                    if reusable_rule_scope_matches(
                        &rule.lifetime,
                        self.id,
                        project.as_deref(),
                        manager_id,
                        candidate.project.as_deref(),
                    ) && permission_rule_covers_request(&rule, &candidate.request)
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
            candidate
                .event_tx
                .try_send(AgentEvent::PermissionRequestResolved {
                    request_id: candidate.request.id.clone(),
                    source_request_id: request_id.to_owned(),
                });
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

    fn check_inner(
        &self,
        tool: &ToolKey,
        scopes: &[&str],
        force_prompt: bool,
        plan_path: Option<&Path>,
        include_builtin_allows: bool,
    ) -> PermissionCheck {
        let session = self.session_rules();
        let plugin = self.plugin_rules.snapshot();
        let project = self.project();

        // Any matching deny wins. No specificity hierarchy — a Wildcard
        // deny blocks everything, a tool-specific deny blocks that tool.
        let mut unclaimed_scopes: Vec<&str> = if force_prompt {
            Vec::new()
        } else {
            Vec::with_capacity(scopes.len())
        };

        for scope in scopes {
            let mut has_allow = false;
            for r in session
                .iter()
                .chain(&self.config_rules)
                .chain(
                    project
                        .builtin_rules
                        .iter()
                        .filter(|_| include_builtin_allows),
                )
                .chain(&plugin)
            {
                if !matches_rule(&r.tool, tool) || !rule_matches_scope(r, tool, scope) {
                    continue;
                }
                match r.effect {
                    Effect::Deny => {
                        info!(tool = %tool, scope = %scope, "permission denied");
                        return PermissionCheck::Denied;
                    }
                    Effect::Allow => {
                        has_allow = true;
                    }
                }
            }

            if has_allow {
                // allow wins for this scope (no deny matched)
            } else if !force_prompt {
                unclaimed_scopes.push(scope);
            }
            // force_prompt: all scopes will be prompted anyway
        }

        if self.yolo.load(Ordering::Relaxed) {
            return PermissionCheck::Allowed;
        }

        let pending: Vec<&str> = if force_prompt {
            scopes.to_vec()
        } else {
            unclaimed_scopes
        };

        if pending.is_empty() {
            return PermissionCheck::Allowed;
        }

        // Plan file auto-allow: fires AFTER deny rules have been evaluated.
        // Only triggers if ALL pending scopes match the plan file path.
        // A single non-plan scope means we must prompt for the rest.
        if !force_prompt && !pending.is_empty() {
            let is_plan_write = plan_path.is_some_and(|pp| {
                matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
                    && {
                        let normalized_plan = normalize_scope_path(&pp.display().to_string());
                        pending
                            .iter()
                            .all(|s| normalize_scope_path(s) == normalized_plan)
                    }
            });
            if is_plan_write {
                return PermissionCheck::Allowed;
            }
        }

        let eff = self
            .tool_defaults
            .get(tool)
            .copied()
            .or_else(|| {
                // McpTool falls back to McpServer-level default (Arc clone, ~2ns)
                let server = match tool {
                    ToolKey::McpTool { server, .. } => server,
                    _ => return None,
                };
                self.tool_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied()
            })
            .unwrap_or(self.default);
        match eff {
            DefaultEffect::Deny => {
                info!(tool = %tool, "denied by default");
                PermissionCheck::Denied
            }
            DefaultEffect::Allow => PermissionCheck::Allowed,
            DefaultEffect::Prompt => PermissionCheck::NeedsPrompt {
                tool: tool.clone(),
                scopes: pending.into_iter().map(|s| s.to_string()).collect(),
                force_prompt,
            },
        }
    }

    pub fn check(&self, tool: &ToolKey, scope: &str, plan_path: Option<&Path>) -> PermissionCheck {
        self.check_inner(tool, &[scope], false, plan_path, true)
    }

    pub fn check_multi(
        &self,
        tool: &ToolKey,
        scopes: &[&str],
        force_prompt: bool,
        plan_path: Option<&Path>,
    ) -> PermissionCheck {
        self.check_inner(tool, scopes, force_prompt, plan_path, true)
    }

    pub fn add_session_rule(&self, rule: PermissionRule) {
        let mut rules = self.session_rules();
        let exists = rules
            .iter()
            .any(|r| r.tool == rule.tool && r.scope == rule.scope && r.effect == rule.effect);
        if !exists {
            rules.push(rule);
        }
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

    pub fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        let mut rules = self.session_rules().clone();
        rules.extend(
            self.inactive_session_allows
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .cloned(),
        );
        rules
    }

    pub fn load_session_rules(&self, rules: Vec<PermissionRule>) {
        let (inactive_allows, active_denies): (Vec<_>, Vec<_>) = rules
            .into_iter()
            .partition(|rule| rule.effect == Effect::Allow);
        *self.session_rules() = active_denies;
        *self
            .inactive_session_allows
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = inactive_allows;
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

    pub fn review_candidates(&self) -> Vec<PermissionReviewCandidate> {
        let mut candidates = self.review_candidates.clone();
        candidates.extend(
            self.inactive_session_allows
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .map(|rule| PermissionReviewCandidate {
                    source: caudra_config::PermissionSource::Conversation,
                    kind: caudra_config::PermissionReviewKind::Rule,
                    tool: Some(rule.tool.clone()),
                    scope: rule.scope.clone(),
                }),
        );
        candidates
    }

    pub fn effective_legacy_policy(&self) -> Vec<EffectivePermissionRule> {
        let builtin_rules = self.project().builtin_rules.clone();
        let mut entries: Vec<_> = self
            .session_rules()
            .iter()
            .cloned()
            .map(|rule| EffectivePermissionRule {
                source: "conversation",
                rule,
                removable: true,
            })
            .collect();
        entries.extend(
            self.config_rules
                .iter()
                .filter(|rule| rule.effect == Effect::Deny)
                .cloned()
                .map(|rule| EffectivePermissionRule {
                    source: "configuration",
                    rule,
                    removable: false,
                }),
        );
        entries.extend(
            builtin_rules
                .iter()
                .cloned()
                .map(|rule| EffectivePermissionRule {
                    source: "builtin",
                    rule,
                    removable: false,
                }),
        );
        entries.extend(self.plugin_rules.snapshot().into_iter().map(|rule| {
            EffectivePermissionRule {
                source: "trusted plugin",
                rule,
                removable: false,
            }
        }));
        entries
    }

    pub fn remove_conversation_legacy_rule(
        &self,
        tool: &ToolKey,
        scope: Option<&str>,
        effect: Effect,
    ) -> bool {
        fn remove(
            rules: &mut Vec<PermissionRule>,
            tool: &ToolKey,
            scope: Option<&str>,
            effect: Effect,
        ) -> bool {
            let before = rules.len();
            rules.retain(|rule| {
                rule.tool != *tool || rule.scope.as_deref() != scope || rule.effect != effect
            });
            rules.len() != before
        }

        if effect == Effect::Allow {
            remove(
                &mut self
                    .inactive_session_allows
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
                tool,
                scope,
                effect,
            )
        } else {
            remove(&mut self.session_rules(), tool, scope, effect)
        }
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

    fn applicable_structured_rules(
        &self,
    ) -> Result<Vec<StructuredPermissionRule>, PermissionPolicyError> {
        self.ensure_conversation_policy_valid()?;
        let mut rules: Vec<_> = self
            .structured_conversation_rules()
            .iter()
            .filter(|record| record.is_active())
            .map(|record| record.rule.clone())
            .collect();
        rules.extend(
            self.persistent_records()?
                .into_iter()
                .map(|record| record.rule),
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

    fn commit_structured_decision(
        &self,
        request: &PermissionRequest,
        answer: &PermissionAnswer,
        approved_project: Option<&Path>,
    ) -> Result<Option<StructuredPermissionRule>, PermissionPolicyError> {
        let (option_id, lifetime) = match answer {
            PermissionAnswer::AllowOnce => ("allow_exact", PermissionLifetime::Once),
            PermissionAnswer::AllowSession => ("allow_exact", PermissionLifetime::Conversation),
            PermissionAnswer::AllowAlwaysLocal => ("allow_exact", PermissionLifetime::Project),
            PermissionAnswer::AllowAlwaysGlobal => ("allow_exact", PermissionLifetime::Global),
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => (option_id.as_str(), lifetime.clone()),
            PermissionAnswer::DenyAlwaysLocal => ("deny_exact", PermissionLifetime::Project),
            PermissionAnswer::DenyAlwaysGlobal => ("deny_exact", PermissionLifetime::Global),
            PermissionAnswer::Deny | PermissionAnswer::DenyWithGuidance(_) => return Ok(None),
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
            StructuredPermissionEffect::Deny => permission_rule_intersects_request(&rule, request),
        };
        if !covers {
            return Err(PermissionPolicyError(format!(
                "authority {option_id:?} does not cover the pending request"
            )));
        }
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

    pub fn apply_decision(&self, tool: &ToolKey, scopes: &[String], answer: &PermissionAnswer) {
        let resolved = if tool.is_mcp() {
            scopes
                .iter()
                .map(|scope| canonical_mcp_scope(scope))
                .collect()
        } else {
            scopes.to_vec()
        };

        match answer {
            PermissionAnswer::AllowOnce
            | PermissionAnswer::Deny
            | PermissionAnswer::DenyWithGuidance(_) => {}
            PermissionAnswer::AllowOption { lifetime, .. } => match lifetime {
                PermissionLifetime::Once => {}
                PermissionLifetime::Conversation => {
                    for s in &resolved {
                        self.add_session_rule(PermissionRule {
                            tool: tool.clone(),
                            scope: Some(s.clone()),
                            effect: Effect::Allow,
                        });
                    }
                }
                PermissionLifetime::Project | PermissionLifetime::Global => {
                    for s in &resolved {
                        self.add_session_rule(PermissionRule {
                            tool: tool.clone(),
                            scope: Some(s.clone()),
                            effect: Effect::Allow,
                        });
                    }
                }
            },
            PermissionAnswer::AllowSession => {
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(s.clone()),
                        effect: Effect::Allow,
                    });
                }
            }
            PermissionAnswer::AllowAlwaysLocal
            | PermissionAnswer::AllowAlwaysGlobal
            | PermissionAnswer::DenyAlwaysLocal
            | PermissionAnswer::DenyAlwaysGlobal => {
                let effect = if answer.is_allow() {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
        }
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
        let scope_refs: Vec<&str> = scopes.scopes.iter().map(|s| s.as_str()).collect();
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
                intent.scopes.scopes = request_scopes;
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
        let initial_request =
            make_request(tool.clone(), scopes.scopes.clone(), scopes.force_prompt);
        let exact_plan_write = plan_path.is_some_and(|plan_path| {
            matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
                && !initial_request.resources.is_empty()
                && initial_request.resources.iter().all(|resource| {
                    resource.access == Some(PermissionResourceAccess::Write)
                        && normalize_scope_path(&resource.value)
                            == normalize_scope_path(&plan_path.display().to_string())
                })
        });
        let force_prompt = scopes.force_prompt
            || (!exact_plan_write
                && initial_request
                    .resources
                    .iter()
                    .any(|resource| resource.requires_prompt));
        let full_request = if force_prompt == scopes.force_prompt {
            initial_request
        } else {
            make_request(tool.clone(), scopes.scopes.clone(), force_prompt)
        };
        let structured_rules = self.applicable_structured_rules().map_err(|error| {
            warn!(%error, "structured permission policy failed closed");
            deny(DECISION_SOURCE_RULE, Some(error.to_string()))
        })?;
        if structured_rules
            .iter()
            .any(|rule| permission_rule_intersects_request(rule, &full_request))
        {
            return Err(deny(DECISION_SOURCE_RULE, None));
        }

        let (pt, ps, force_prompt) = match self.check_inner(
            tool,
            &scope_refs,
            force_prompt,
            plan_path,
            include_builtin_allows,
        ) {
            PermissionCheck::Allowed => return allowed(by_rule()),
            PermissionCheck::Denied => return Err(deny(DECISION_SOURCE_RULE, None)),
            PermissionCheck::NeedsPrompt {
                tool,
                scopes,
                force_prompt,
            } => (tool, scopes, force_prompt),
        };

        let refs: Vec<&str> = ps.iter().map(|s| s.as_str()).collect();
        let (t2, s2) =
            match self.check_inner(&pt, &refs, force_prompt, plan_path, include_builtin_allows) {
                PermissionCheck::Allowed => return allowed(by_rule()),
                PermissionCheck::Denied => return Err(deny(DECISION_SOURCE_RULE, None)),
                PermissionCheck::NeedsPrompt { tool, scopes, .. } => (tool, scopes),
            };

        let request = make_request(t2.clone(), s2.clone(), force_prompt);
        let structured_rules = self.applicable_structured_rules().map_err(|error| {
            warn!(%error, "structured permission policy failed closed");
            deny(DECISION_SOURCE_RULE, Some(error.to_string()))
        })?;
        match evaluate_structured_permission_rules(&structured_rules, &request) {
            StructuredPermissionDecision::Deny => {
                return Err(deny(DECISION_SOURCE_RULE, None));
            }
            StructuredPermissionDecision::Allow => return allowed(DECISION_SOURCE_RULE),
            StructuredPermissionDecision::NoMatch => {}
        }

        let Some(_) = user_response_rx else {
            warn!(tool = %tool, scope = %scope_display(), "no permission response channel");
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

        let (answer_tx, answer_rx) = flume::bounded(1);
        {
            let mut pending = self.pending();
            let current_rules = self.applicable_structured_rules().map_err(|error| {
                warn!(%error, "structured permission policy failed closed");
                deny(DECISION_SOURCE_RULE, Some(error.to_string()))
            })?;
            match evaluate_structured_permission_rules(&current_rules, &request) {
                StructuredPermissionDecision::Deny => {
                    return Err(deny(DECISION_SOURCE_RULE, None));
                }
                StructuredPermissionDecision::Allow => return allowed(DECISION_SOURCE_RULE),
                StructuredPermissionDecision::NoMatch => {}
            }
            let requests = pending.entry(self.id).or_default();
            if requests.contains_key(request_id) {
                warn!(request_id, "duplicate permission request id");
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            requests.insert(
                request_id.to_owned(),
                PendingPermission {
                    request: request.clone(),
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
        }
        let response = cancel.race(answer_rx.recv_async()).await;
        self.remove_pending(request_id);

        let decision = match response {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                warn!(tool = %tool, scope = %scope_display(), "permission channel closed");
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            Err(_) => return Err(deny(DECISION_SOURCE_USER_ABORT, None)),
        };

        let allow = match &decision {
            PendingDecision::Explicit(answer) => answer.is_allow(),
            PendingDecision::MatchedRule => true,
        };
        if allow {
            let current_rules = self.applicable_structured_rules().map_err(|error| {
                warn!(%error, "structured permission policy failed closed");
                deny(DECISION_SOURCE_RULE, Some(error.to_string()))
            })?;
            if current_rules
                .iter()
                .any(|rule| permission_rule_intersects_request(rule, &request))
            {
                return Err(deny(DECISION_SOURCE_RULE, None));
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

fn rule_matches_scope(rule: &PermissionRule, tool: &ToolKey, scope: &str) -> bool {
    match &rule.scope {
        None => true,
        Some(pattern) if tool.is_mcp() => {
            scope_matches(pattern, scope)
                || (canonical_mcp_scope(pattern) == canonical_mcp_scope(scope))
        }
        Some(pattern) => {
            scope_matches(pattern, scope)
                || matches!(tool, ToolKey::Native(name) if matches!(name.as_ref(), "bash" | "shell"))
                    && bash_command_scope(scope)
                        .is_some_and(|command| scope_matches(pattern, command))
        }
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

fn bash_command_scope(scope: &str) -> Option<&str> {
    bash_scope_parts(scope).map(|(command, _)| command)
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

fn canonical_mcp_scope(scope: &str) -> String {
    serde_json::from_str(scope)
        .map(|value| canonical_json(&value))
        .unwrap_or_else(|_| scope.to_owned())
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
    use caudra_storage::permission_state::PERMISSION_STATE_FILE;
    use test_case::test_case;

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

    fn mgr_with(config: PermissionsConfig, cwd: PathBuf) -> PermissionManager {
        PermissionManager::new_nonpersistent(config, cwd, Arc::default())
    }

    fn default_mgr() -> PermissionManager {
        mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"))
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
            PathBuf::from("/tmp"),
        );
        let check = mgr.check_multi(&ToolKey::native("bash"), &scopes, false, None);
        assert_eq!(matches!(check, PermissionCheck::Allowed), expect_allowed);
    }

    #[test]
    fn compound_denied_if_any_segment_denied() {
        let mgr = mgr_with(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("bash"),
                &["cd /tmp", "cargo test", "rm -rf /"],
                false,
                None
            ),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        let mgr = mgr_with(make_config(vec![allow_rule("*")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi(&ToolKey::native("bash"), &["echo $(whoami)"], true, None),
            PermissionCheck::NeedsPrompt { .. }
        ));
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
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), &scope, None,),
            PermissionCheck::Denied
        ));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        matches!(
            default_mgr().check(&ToolKey::native(tool), scope, None),
            PermissionCheck::Allowed
        )
    }

    #[test]
    fn builtin_allows_apply_only_to_bundled_implementations() {
        let manager = default_mgr();
        let tool = ToolKey::native("task");
        assert!(matches!(
            manager.check_inner(&tool, &["{}"], false, None, true),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            manager.check_inner(&tool, &["{}"], false, None, false),
            PermissionCheck::NeedsPrompt { .. }
        ));
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
        assert!(matches!(
            default_mgr().check(&ToolKey::native("write"), &path, None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn session_rule_overrides_config() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        mgr.add_session_rule(deny_rule("cargo *"));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn deny_overrides_default_allow() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![deny_rule("rm *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn allow_decision_is_exact() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test --all".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test --all", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo build", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn deny_decision_uses_exact() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test".into()],
            &PermissionAnswer::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo build", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
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
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("hint".into()),
        ] {
            assert_eq!(PermissionAnswer::decode(&a.encode()), Some(a));
        }
    }

    #[test]
    fn check_multi_force_prompt_skips_allow_rules() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *"), allow_rule("git *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("bash"),
                &["cargo test", "git push"],
                false,
                None
            ),
            PermissionCheck::Allowed
        ));
        match mgr.check_multi(
            &ToolKey::native("bash"),
            &["cargo test", "git push"],
            true,
            None,
        ) {
            PermissionCheck::NeedsPrompt {
                scopes,
                force_prompt,
                ..
            } => {
                assert_eq!(scopes, vec!["cargo test", "git push"]);
                assert!(force_prompt);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn check_multi_deny_wins_over_force_prompt() {
        let mgr = mgr_with(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi(&ToolKey::native("bash"), &["rm -rf /"], true, None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn check_multi_partial_coverage_prompts_uncovered() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        match mgr.check_multi(
            &ToolKey::native("bash"),
            &["cargo test", "git push", "ls"],
            false,
            None,
        ) {
            PermissionCheck::NeedsPrompt { scopes, .. } => {
                assert_eq!(scopes, vec!["git push", "ls"]);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn apply_decision_multi_scope_keeps_each_exact() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test".into(), "git status".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "git status", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "git push", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn mcp_remembered_allow_reuses_only_exact_canonical_input() {
        let mgr = default_mgr();
        let approved = canonical_json(&serde_json::json!({
            "url": "https://a",
            "options": {"format": "json", "limit": 10}
        }));
        mgr.apply_decision(
            &ToolKey::parse("myfetch.search").unwrap(),
            &[approved],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check(
                &ToolKey::parse("myfetch.search").unwrap(),
                r#"{"options":{"limit":10,"format":"json"},"url":"https://a"}"#,
                None
            ),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(
                &ToolKey::parse("myfetch.search").unwrap(),
                r#"{"url":"https://b","options":{"format":"json","limit":10}}"#,
                None
            ),
            PermissionCheck::NeedsPrompt { .. }
        ));
        assert!(matches!(
            mgr.check(
                &ToolKey::parse("myfetch.exec").unwrap(),
                "{\"cmd\":\"ls\"}",
                None
            ),
            PermissionCheck::NeedsPrompt { .. }
        ));
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
                    &crate::tools::PermissionScopes::single(scope),
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
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "anything", None),
            PermissionCheck::Denied
        ));
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
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "ls", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("write"), "/tmp/x", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn mcp_remembered_deny_is_exact_without_a_broad_option() {
        let mgr = mgr_with(make_config(vec![]), PathBuf::from("/tmp"));
        let tool = ToolKey::McpTool {
            server: "deepwiki".into(),
            tool: "search".into(),
        };
        mgr.apply_decision(
            &tool,
            &["{\"q\":\"dangerous\"}".into()],
            &PermissionAnswer::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check(&tool, "{\"q\":\"dangerous\"}", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check(&tool, "{\"q\":\"safe\"}", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn yolo_mode_allows_but_deny_still_blocks() {
        let mgr = mgr_with(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        mgr.toggle_yolo();
        assert!(mgr.is_yolo());
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
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

    #[test]
    fn add_session_rule_is_idempotent() {
        let mgr = default_mgr();
        let rule = allow_rule("cargo *");
        mgr.add_session_rule(rule.clone());
        mgr.add_session_rule(rule.clone());
        mgr.add_session_rule(rule);
        assert_eq!(mgr.session_rules_snapshot().len(), 1);
    }

    #[test]
    fn restored_legacy_allows_are_inactive_review_candidates() {
        let mgr = default_mgr();
        mgr.load_session_rules(vec![allow_rule("cargo *"), deny_rule("rm *")]);

        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /tmp/x", None),
            PermissionCheck::Denied
        ));
        assert_eq!(mgr.session_rules_snapshot().len(), 2);
        assert!(mgr.review_candidates().iter().any(|candidate| {
            candidate.source == caudra_config::PermissionSource::Conversation
                && candidate.scope.as_deref() == Some("cargo *")
        }));
        assert!(mgr.remove_conversation_legacy_rule(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
        ));
        assert!(mgr.remove_conversation_legacy_rule(
            &ToolKey::native("bash"),
            Some("rm *"),
            Effect::Deny,
        ));
        assert!(mgr.session_rules_snapshot().is_empty());
    }

    #[test_case(PermissionAnswer::AllowOnce ; "allow_once")]
    #[test_case(PermissionAnswer::Deny ; "deny_once")]
    fn once_decisions_add_no_session_rules(answer: PermissionAnswer) {
        let mgr = default_mgr();
        mgr.apply_decision(&ToolKey::native("bash"), &["cargo test".into()], &answer);
        assert!(mgr.session_rules_snapshot().is_empty());
    }

    #[test]
    fn default_deny_blocks_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_deny_with_allow_rules() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                rules: vec![allow_rule("cargo *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_allow_allows_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn default_prompt_is_default_behavior() {
        let mgr = mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

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
        assert!(matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: "search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: "web_search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
        ));
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
        assert!(!matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "github".into(),
                    tool: "search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
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
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("write"), "/etc/passwd", None),
            PermissionCheck::Denied
        ));
    }

    #[test_case("write", true ; "write_tool_allowed")]
    #[test_case("edit", true ; "edit_tool_allowed")]
    #[test_case("bash", false ; "non_write_tool_prompts")]
    fn plan_path_auto_allows_file_write_tools_only(tool: &str, expect_allowed: bool) {
        let plan = "/home/user/.local/state/caudra/plans/test.md";
        let plan_path = Path::new(plan);
        let mgr = default_mgr();
        assert_eq!(
            matches!(
                mgr.check(&ToolKey::native(tool), plan, Some(plan_path)),
                PermissionCheck::Allowed
            ),
            expect_allowed,
        );
    }

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
        for m in [&mgr, &fork] {
            assert!(matches!(
                m.check(&ToolKey::native("edit"), "/x/f", None),
                PermissionCheck::Allowed
            ));
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
        assert!(matches!(
            mgr.check(&ToolKey::native("edit"), "/x/f", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn plan_path_multi_scope_all_must_match() {
        let plan = "/home/user/.local/state/caudra/plans/test.md";
        let plan_path = Path::new(plan);
        let mgr = default_mgr();

        // All scopes match plan → allowed
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("write"),
                &[plan, plan],
                false,
                Some(plan_path),
            ),
            PermissionCheck::Allowed
        ));

        // One scope is non-plan → needs prompt
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("write"),
                &[plan, "/etc/passwd"],
                false,
                Some(plan_path),
            ),
            PermissionCheck::NeedsPrompt { .. }
        ));
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

            let bytes = std::fs::read(state_dir.path().join(PERMISSION_STATE_FILE)).unwrap();
            let serialized = String::from_utf8(bytes).unwrap();
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
            let stored =
                std::fs::read_to_string(state_dir.path().join(PERMISSION_STATE_FILE)).unwrap();
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

            assert!(matches!(
                manager.check(&ToolKey::native("write"), &first_file, None),
                PermissionCheck::Allowed
            ));
            assert!(matches!(
                manager.check(&ToolKey::native("write"), &second_file, None),
                PermissionCheck::NeedsPrompt { .. }
            ));
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
            assert!(matches!(
                manager.check(&ToolKey::native("write"), &first_file, None),
                PermissionCheck::NeedsPrompt { .. }
            ));
            assert!(matches!(
                manager.check(&ToolKey::native("write"), &second_file, None),
                PermissionCheck::Allowed
            ));
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
            std::fs::create_dir(&state_path).unwrap();
            let corrupt = b"{corrupt permission state";
            std::fs::write(state_path.join(PERMISSION_STATE_FILE), corrupt).unwrap();
            let manager = persistent_manager(StateDir::from_path(state_path.clone()), &project);

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
                std::fs::read(state_path.join(PERMISSION_STATE_FILE)).unwrap(),
                corrupt
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
            std::fs::rename(&state_path, temp.path().join("old-state")).unwrap();
            std::fs::write(&state_path, b"blocks directory recreation").unwrap();
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
}
