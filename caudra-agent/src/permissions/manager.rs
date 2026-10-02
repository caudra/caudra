use super::editor::{PermissionAuthorityProvider, PermissionPublication};
use super::{
    BOUNDARY_UNVERIFIABLE_PREFIX, ConfiguredPolicy, PermissionBroker, PermissionLifetime,
    PermissionPolicyError, PermissionRuleRecord, PluginRuleStore, SharedPermissionState,
    builtin_rules, configured_policy, physical_boundary_check, shared_policy,
};
use super::{
    PermissionRequest,
    pattern_matching::CompiledPattern,
    pattern_recognition::{
        MAX_RECOGNIZER_BYTES, MAX_RECOGNIZER_SUGGESTIONS, MAX_TIMESTAMP_MS, ObserveOutcome,
        PatternCandidate, PatternRecognizer, RecognizerLimits,
    },
    policy::validate_compiled_templates,
    structured::trusted_command_observation,
};
use crate::decisions::Decisions;
use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
use caudra_storage::permission_patterns::PatternDefinition;
use caudra_storage::permission_state::mutation::{
    PermissionGeneration, PermissionMutation, PermissionOwner, PermissionRecordIdentity,
    PermissionSnapshot, prepare_mutation,
};
use caudra_storage::permission_state::validate_conversation_record;
use caudra_storage::sessions::{PermissionMode, SessionDatabase};
use caudra_storage::state::{SCOPE_GLOBAL, StateKey, StateStore};
use caudra_storage::{StateClass, StateDir, now_epoch};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::warn;

pub(super) static NEXT_PERMISSION_MANAGER_ID: AtomicU64 = AtomicU64::new(1);
pub(super) const PERMISSION_POLL_INTERVAL: Duration = Duration::from_millis(500);
const RUNTIME_PATTERN_MIN_SUPPORT: usize = 3;
const MAX_PATTERN_CANDIDATE_BYTES: usize = MAX_RECOGNIZER_BYTES / 8;
const MAX_PATTERN_PROJECTS: usize = 4;
const STALE_PATTERN_CONTEXT: &str = "pattern discovery project or context revision changed";
const INVALID_PATTERN_CONTEXT: &str =
    "pattern candidate does not belong to the current project context";
const STALE_PATTERN_PROPOSAL: &str = "suggested pattern changed or is no longer available";
const PATTERN_DISMISSALS: StateKey = StateKey {
    name: "permission.pattern_dismissals",
    class: StateClass::Persistent,
};
const MAX_PATTERN_DISMISSALS: usize = 256;
const PATTERN_SNOOZE_SECONDS: u64 = 24 * 60 * 60;
const DIGEST_BYTES: usize = 32;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternDismissal {
    project: [u8; DIGEST_BYTES],
    definition: [u8; DIGEST_BYTES],
    expires_at: Option<u64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternDismissals {
    entries: Vec<PatternDismissal>,
}

#[derive(Default)]
pub(super) struct PatternCandidates {
    pub(super) proposals: Vec<PatternCandidate>,
    dismissals: PatternDismissals,
}

impl PatternCandidates {
    pub(super) fn is_dismissed(&self, definition: &PatternDefinition) -> bool {
        !self.dismissals.entries.is_empty()
            && definition.fingerprint().is_ok_and(|id| {
                self.dismissals.contains(
                    &pattern_project_digest(Path::new(&definition.context.path_binding)),
                    &id,
                )
            })
    }
}

#[cfg(test)]
impl From<Vec<PatternCandidate>> for PatternCandidates {
    fn from(proposals: Vec<PatternCandidate>) -> Self {
        Self {
            proposals,
            ..Self::default()
        }
    }
}

impl PatternDismissals {
    fn load(state_dir: &StateDir) -> Self {
        let loaded =
            SessionDatabase::open_read_only(&state_dir.for_class(PATTERN_DISMISSALS.class))
                .and_then(|database| database.state_get(SCOPE_GLOBAL, PATTERN_DISMISSALS.name));
        let mut settings: Self = match loaded {
            Ok(settings) => settings.unwrap_or_default(),
            Err(_) => {
                warn!("pattern dismissal preferences could not be read");
                Self::default()
            }
        };
        settings.prune(now_epoch());
        settings
    }

    fn prune(&mut self, now: u64) {
        self.entries.retain(|entry| {
            entry
                .expires_at
                .is_none_or(|expiry| expiry > now && expiry <= MAX_TIMESTAMP_MS / 1000)
        });
        let excess = self.entries.len().saturating_sub(MAX_PATTERN_DISMISSALS);
        self.entries.drain(..excess);
    }

    fn contains(&self, project: &[u8; DIGEST_BYTES], definition_id: &str) -> bool {
        let definition: [u8; DIGEST_BYTES] = Sha256::digest(definition_id.as_bytes()).into();
        self.entries
            .iter()
            .any(|entry| &entry.project == project && entry.definition == definition)
    }

    fn insert(&mut self, entry: PatternDismissal, now: u64) {
        self.entries.retain(|existing| {
            existing.project != entry.project || existing.definition != entry.definition
        });
        self.entries.push(entry);
        self.prune(now);
    }
}

fn pattern_project_digest(project: &Path) -> [u8; DIGEST_BYTES] {
    Sha256::digest(project.as_os_str().as_encoded_bytes()).into()
}

#[derive(Default)]
struct PatternDiscovery {
    recognizer: Option<PatternRecognizer>,
    learned: Vec<PatternCandidate>,
    supplied: Vec<PatternCandidate>,
}

fn discovery_for_project<'a>(
    projects: &'a mut BTreeMap<PathBuf, PatternDiscovery>,
    project: &Path,
) -> &'a mut PatternDiscovery {
    if !projects.contains_key(project) && projects.len() == MAX_PATTERN_PROJECTS {
        projects.pop_first();
    }
    projects.entry(project.to_path_buf()).or_default()
}

fn bounded_candidates(
    candidates: impl IntoIterator<Item = PatternCandidate>,
) -> Vec<PatternCandidate> {
    let mut bytes = 0;
    let mut retained = Vec::new();
    for candidate in candidates.into_iter().take(MAX_RECOGNIZER_SUGGESTIONS) {
        if CompiledPattern::compile(&candidate.definition).is_err() {
            continue;
        }
        let Ok(encoded) = serde_json::to_vec(&candidate) else {
            continue;
        };
        if bytes + encoded.len() > MAX_PATTERN_CANDIDATE_BYTES {
            continue;
        }
        bytes += encoded.len();
        retained.push(candidate);
    }
    retained
}

pub struct PermissionManager {
    pub(super) id: u64,
    pub(super) context_revision: RwLock<u64>,
    pub(super) structured_conversation_rules: Mutex<Vec<PermissionRuleRecord>>,
    pub(super) conversation_policy_error: Mutex<Option<String>>,
    pub(super) conversation_snapshot: Mutex<Option<PermissionSnapshot>>,
    pub(super) publication: RwLock<Option<Arc<dyn PermissionPublication>>>,
    pub(super) editor_provider: RwLock<Option<Arc<dyn PermissionAuthorityProvider>>>,
    last_permission_poll: Mutex<Option<Instant>>,
    external_generation: Mutex<Option<PermissionGeneration>>,
    pub(super) broker: Arc<PermissionBroker>,
    pub(super) configured: RwLock<ConfiguredPolicy>,
    permission_mode: Mutex<PermissionModeState>,
    decisions: Arc<RwLock<Option<Decisions>>>,
    pub(super) project: Mutex<ProjectContext>,
    pub(super) policy: Option<Arc<SharedPermissionState>>,
    pub(super) plugin_rules: Arc<PluginRuleStore>,
    patterns: Arc<Mutex<BTreeMap<PathBuf, PatternDiscovery>>>,
    pattern_dismissals: Arc<Mutex<PatternDismissals>>,
}

#[derive(Clone)]
struct PermissionModeState {
    seed: PermissionMode,
    stored: Option<PermissionMode>,
    /// Without the decision engine a seeded or stored Auto acts as Ask,
    /// whatever supplied it, and the stored choice survives for a process
    /// that has the engine again.
    decision_engine: bool,
}

impl PermissionModeState {
    fn mode(&self) -> PermissionMode {
        match self.stored.as_ref().unwrap_or(&self.seed) {
            PermissionMode::Auto if !self.decision_engine => PermissionMode::Ask,
            mode => mode.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct ProjectContext {
    pub(super) cwd: PathBuf,
    pub(super) canonical_project: Option<PathBuf>,
    pub(super) policy_context_error: Option<String>,
    pub(super) builtin_rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokedRuleScope {
    Conversation,
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionProjectFilter {
    Current,
    All,
    Project(PathBuf),
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

    pub(super) fn build(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
        policy: Option<Arc<SharedPermissionState>>,
        policy_context_error: Option<String>,
    ) -> Self {
        let seed = PermissionMode::from(config.yolo);
        let decision_engine = config.decision_engine;
        let configured = configured_policy(config, None);
        let builtin_rules = builtin_rules(&cwd, policy.as_ref().map(|state| &state.state_dir));

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

        let broker = policy
            .as_ref()
            .map(|state| Arc::clone(&state.broker))
            .unwrap_or_default();
        let pattern_dismissals = policy
            .as_ref()
            .map(|state| PatternDismissals::load(&state.state_dir))
            .unwrap_or_default();
        plugin_rules.observe(&broker);
        Self {
            id: NEXT_PERMISSION_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
            context_revision: RwLock::new(0),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            conversation_snapshot: Mutex::new(None),
            publication: RwLock::new(None),
            editor_provider: RwLock::new(None),
            last_permission_poll: Mutex::new(None),
            external_generation: Mutex::new(None),
            broker,
            configured: RwLock::new(configured),
            permission_mode: Mutex::new(PermissionModeState {
                seed,
                stored: None,
                decision_engine,
            }),
            decisions: Arc::default(),
            project: Mutex::new(ProjectContext {
                cwd,
                canonical_project: None,
                policy_context_error,
                builtin_rules,
            }),
            policy,
            plugin_rules,
            patterns: Arc::default(),
            pattern_dismissals: Arc::new(Mutex::new(pattern_dismissals)),
        }
    }

    pub(super) fn with_canonical_project(mut self, canonical_project: Option<PathBuf>) -> Self {
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

    fn state_dir(&self) -> Option<&StateDir> {
        self.policy.as_ref().map(|state| &state.state_dir)
    }

    pub(super) fn project(&self) -> MutexGuard<'_, ProjectContext> {
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
        let mut revision = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *revision += 1;
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
            builtin_rules: builtin_rules(cwd, self.state_dir()),
        };
        self.notify_policy_changed("");
    }

    pub fn set_project_with_config(&self, cwd: &Path, config: PermissionsConfig) {
        let mut revision = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *revision += 1;
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
            builtin_rules: builtin_rules(cwd, self.state_dir()),
        };
        self.notify_policy_changed("");
    }

    /// Fresh manager for a new session runtime: shares config and builtin
    /// rules plus the current yolo state, but owns empty session rules so
    /// restoring one session never clobbers another's grants.
    pub fn fork(&self) -> Self {
        let _context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let project = self.project().clone();
        let configured = self.configured().clone();
        Self {
            id: NEXT_PERMISSION_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
            context_revision: RwLock::new(0),
            structured_conversation_rules: Mutex::new(Vec::new()),
            conversation_policy_error: Mutex::new(None),
            conversation_snapshot: Mutex::new(None),
            publication: RwLock::new(None),
            editor_provider: RwLock::new(
                self.editor_provider
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
            ),
            last_permission_poll: Mutex::new(None),
            external_generation: Mutex::new(None),
            broker: Arc::clone(&self.broker),
            configured: RwLock::new(configured),
            permission_mode: Mutex::new(self.permission_mode().clone()),
            decisions: Arc::clone(&self.decisions),
            project: Mutex::new(project),
            policy: self.policy.clone(),
            plugin_rules: Arc::clone(&self.plugin_rules),
            patterns: Arc::clone(&self.patterns),
            pattern_dismissals: Arc::clone(&self.pattern_dismissals),
        }
    }

    pub fn fork_session(&self) -> Self {
        let mut manager = self.fork();
        manager.decisions = Arc::new(RwLock::new(
            manager.decisions().map(|service| service.fresh_session()),
        ));
        manager
    }

    pub(super) fn structured_conversation_rules(
        &self,
    ) -> MutexGuard<'_, Vec<PermissionRuleRecord>> {
        self.structured_conversation_rules
            .lock()
            .unwrap_or_else(|error| {
                warn!("structured permission mutex was poisoned, recovering");
                error.into_inner()
            })
    }

    fn permission_mode(&self) -> MutexGuard<'_, PermissionModeState> {
        self.permission_mode
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn update_mode<T>(&self, update: impl FnOnce(&mut PermissionModeState) -> T) -> T {
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let result = update(&mut self.permission_mode());
        self.notify_policy_changed("");
        result
    }

    pub fn mode(&self) -> PermissionMode {
        self.permission_mode().mode()
    }

    pub fn decisions(&self) -> Option<Decisions> {
        self.decisions
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Ignores a service while the decision engine is off, so no caller can
    /// attach one the startup policy refused.
    pub fn set_decisions(&self, decisions: Option<Decisions>) {
        let decisions = decisions.filter(|_| self.decision_engine());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *self
            .decisions
            .write()
            .unwrap_or_else(|error| error.into_inner()) = decisions;
        self.notify_policy_changed("");
    }

    pub fn set_seed_mode(&self, seed: PermissionMode) {
        self.update_mode(|state| state.seed = seed);
    }

    pub(crate) fn passive_decision_revision(&self) -> Option<u64> {
        let revision = self.broker.revision.load(Ordering::Acquire);
        self.passive_decision_is_current(revision)
            .then_some(revision)
    }

    pub(crate) fn passive_decision_is_current(&self, revision: u64) -> bool {
        !self.is_yolo() && self.broker.revision.load(Ordering::Acquire) == revision
    }

    pub fn set_session_mode(&self, stored: Option<PermissionMode>) {
        self.update_mode(|state| state.stored = stored);
    }

    pub fn persisted_mode(&self) -> Option<PermissionMode> {
        self.permission_mode().stored.clone()
    }

    fn toggle_mode(&self, mode: PermissionMode) -> bool {
        self.update_mode(|state| {
            let enabled = state.mode() != mode;
            state.stored = Some(if enabled { mode } else { PermissionMode::Ask });
            enabled
        })
    }

    /// Refused, leaving the mode alone, while the decision engine is off.
    pub fn toggle_auto(&self) -> bool {
        self.decision_engine() && self.toggle_mode(PermissionMode::Auto)
    }

    /// Whether the decision engine experiment is on, which Auto needs.
    pub fn decision_engine(&self) -> bool {
        self.permission_mode().decision_engine
    }

    pub fn toggle_yolo(&self) -> bool {
        self.toggle_mode(PermissionMode::Yolo)
    }

    pub fn is_yolo(&self) -> bool {
        self.mode() == PermissionMode::Yolo
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
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.structured_conversation_rules().clone()
    }

    pub fn conversation_permission_snapshot(&self) -> Option<PermissionSnapshot> {
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.conversation_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn load_structured_conversation_rules(&self, rules: Vec<PermissionRuleRecord>) {
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(publication) = self.publication() {
            let result = publication.snapshot().and_then(|snapshot| {
                if snapshot.records != rules {
                    return Err(super::editor::PermissionEditError::Conflict);
                }
                self.publish_conversation_snapshot(snapshot)
            });
            if let Err(error) = result {
                *self
                    .conversation_policy_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
            }
            self.notify_policy_changed("");
            return;
        }
        let error = rules.iter().find_map(|rule| {
            validate_conversation_record(rule)
                .map_err(|error| PermissionPolicyError(error.to_string()))
                .and_then(|()| validate_compiled_templates(&rule.rule))
                .err().map(|error| {
                warn!(%error, rule_id = %rule.id, "conversation permission policy failed closed");
                error.to_string()
            })
        });
        *self
            .conversation_policy_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = error;
        *self.structured_conversation_rules() = rules;
        self.notify_policy_changed("");
    }

    pub fn set_pattern_candidates(&self, candidates: Vec<PatternCandidate>) {
        let (project, revision) = self.pattern_candidate_context();
        if self
            .set_pattern_candidates_for_project(&project, revision, candidates)
            .is_err()
        {
            warn!("pattern candidates rejected for stale or invalid project context");
        }
    }

    pub fn pattern_candidate_context(&self) -> (PathBuf, u64) {
        let revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        (self.project_cwd(), *revision)
    }

    pub fn set_pattern_candidates_for_project(
        &self,
        project: &Path,
        revision: u64,
        candidates: Vec<PatternCandidate>,
    ) -> Result<(), PermissionPolicyError> {
        if candidates.iter().any(|candidate| {
            Path::new(&candidate.definition.context.path_binding) != project
                || !Path::new(&candidate.definition.context.effective_workdir).is_absolute()
                || CompiledPattern::compile(&candidate.definition).is_err()
        }) {
            return Err(PermissionPolicyError(INVALID_PATTERN_CONTEXT.into()));
        }
        let candidates = bounded_candidates(candidates);
        let current_revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if *current_revision != revision || self.project_cwd() != project {
            return Err(PermissionPolicyError(STALE_PATTERN_CONTEXT.into()));
        }
        let changed = {
            let mut projects = self
                .patterns
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let discovery = discovery_for_project(&mut projects, project);
            if discovery.supplied == candidates {
                false
            } else {
                discovery.supplied = candidates;
                true
            }
        };
        drop(current_revision);
        if changed {
            self.notify_policy_changed("");
        }
        Ok(())
    }

    pub(super) fn pattern_candidates(&self) -> PatternCandidates {
        self.pattern_candidates_at(&self.project_cwd(), now_epoch())
    }

    pub fn pattern_proposal_inventory(&self) -> (PathBuf, u64, Vec<PatternCandidate>) {
        let revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let project = self.project_cwd();
        let candidates = self.pattern_candidates_at(&project, now_epoch());
        (project, *revision, candidates.proposals)
    }

    fn pattern_candidates_at(&self, project: &Path, now: u64) -> PatternCandidates {
        let candidates = {
            let projects = self
                .patterns
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            projects
                .get(project)
                .map(|patterns| {
                    patterns
                        .learned
                        .iter()
                        .chain(&patterns.supplied)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let mut dismissals = self
            .pattern_dismissals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        dismissals.prune(now);
        let project_digest = pattern_project_digest(project);
        let mut seen = BTreeSet::new();
        let proposals = bounded_candidates(candidates.into_iter().filter(|candidate| {
            Path::new(&candidate.definition.context.path_binding) == project
                && candidate
                    .definition
                    .fingerprint()
                    .is_ok_and(|id| !dismissals.contains(&project_digest, &id) && seen.insert(id))
        }));
        PatternCandidates {
            proposals,
            dismissals: dismissals.clone(),
        }
    }

    pub fn dismiss_pattern_proposal(
        &self,
        project: &Path,
        revision: u64,
        definition_id: &str,
    ) -> Result<(), PermissionPolicyError> {
        self.hide_pattern_proposal_at(project, revision, definition_id, false, now_epoch())
    }

    pub fn snooze_pattern_proposal(
        &self,
        project: &Path,
        revision: u64,
        definition_id: &str,
    ) -> Result<(), PermissionPolicyError> {
        self.hide_pattern_proposal_at(project, revision, definition_id, true, now_epoch())
    }

    fn hide_pattern_proposal_at(
        &self,
        project: &Path,
        revision: u64,
        definition_id: &str,
        snooze: bool,
        now: u64,
    ) -> Result<(), PermissionPolicyError> {
        let current_revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if *current_revision != revision || self.project_cwd() != project {
            return Err(PermissionPolicyError(STALE_PATTERN_CONTEXT.into()));
        }
        let projects = self
            .patterns
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let available = projects.get(project).is_some_and(|patterns| {
            patterns
                .learned
                .iter()
                .chain(&patterns.supplied)
                .any(|candidate| {
                    candidate
                        .definition
                        .fingerprint()
                        .is_ok_and(|id| id == definition_id)
                })
        });
        if !available {
            return Err(PermissionPolicyError(STALE_PATTERN_PROPOSAL.into()));
        }
        let mut dismissals = self
            .pattern_dismissals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        dismissals.prune(now);
        let project_digest = pattern_project_digest(project);
        if dismissals.contains(&project_digest, definition_id) {
            return Err(PermissionPolicyError(STALE_PATTERN_PROPOSAL.into()));
        }
        let entry = PatternDismissal {
            project: project_digest,
            definition: Sha256::digest(definition_id.as_bytes()).into(),
            expires_at: snooze.then(|| now.saturating_add(PATTERN_SNOOZE_SECONDS)),
        };
        if let Some(policy) = &self.policy {
            let mut store = StateStore::open(&policy.state_dir, PATTERN_DISMISSALS.class)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            *dismissals = store
                .update(
                    SCOPE_GLOBAL,
                    PATTERN_DISMISSALS,
                    |settings: &mut PatternDismissals| {
                        settings.insert(entry, now);
                        settings.clone()
                    },
                )
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
        } else {
            dismissals.insert(entry, now);
        }
        drop(dismissals);
        drop(projects);
        drop(current_revision);
        self.notify_policy_changed("");
        Ok(())
    }

    pub(super) fn observe_pattern_request(&self, request: &PermissionRequest) {
        let revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let project = self.project_cwd();
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return;
        };
        let timestamp_ms = u64::try_from(now.as_millis())
            .unwrap_or(MAX_TIMESTAMP_MS)
            .min(MAX_TIMESTAMP_MS);
        let observations = request
            .resources
            .iter()
            .enumerate()
            .filter_map(|(index, resource)| {
                let mut observation = trusted_command_observation(request, resource)?;
                if Path::new(&observation.context.path_binding) != project {
                    return None;
                }
                observation.source.source_identity = format!("runtime:{}", self.id);
                observation.source.observation_id = format!("{}:{index}", request.id);
                observation.source.session_id = self.id.to_string();
                observation.source.timestamp_ms = timestamp_ms;
                Some(observation)
            })
            .collect::<Vec<_>>();
        if observations.is_empty() {
            return;
        }
        let mut projects = self
            .patterns
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let patterns = discovery_for_project(&mut projects, &project);
        if patterns.recognizer.is_none() {
            patterns.recognizer = PatternRecognizer::new(
                RecognizerLimits {
                    min_support: RUNTIME_PATTERN_MIN_SUPPORT,
                    min_sessions: 1,
                    ..RecognizerLimits::default()
                },
                MAX_TIMESTAMP_MS,
            )
            .ok();
        }
        let Some(recognizer) = &mut patterns.recognizer else {
            return;
        };
        let mut changed = false;
        for observation in observations {
            changed |= matches!(
                recognizer.observe(observation),
                Ok(ObserveOutcome::Added) | Err(_)
            );
        }
        let changed = if changed {
            let learned = bounded_candidates(recognizer.suggestions().unwrap_or_default());
            if patterns.learned == learned {
                false
            } else {
                patterns.learned = learned;
                true
            }
        } else {
            false
        };
        drop(projects);
        drop(revision);
        if changed {
            self.notify_policy_changed("");
        }
    }

    pub(super) fn ensure_conversation_policy_valid(&self) -> Result<(), PermissionPolicyError> {
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
        self.structured_rule_inventory_filtered(&PermissionProjectFilter::Current, false)
    }

    pub fn structured_rule_inventory_filtered(
        &self,
        filter: &PermissionProjectFilter,
        include_revoked: bool,
    ) -> Result<Vec<PermissionRuleRecord>, PermissionPolicyError> {
        self.poll_permission_changes()?;
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_conversation_policy_valid()?;
        let mut inventory: Vec<_> = self
            .structured_conversation_rules()
            .iter()
            .filter(|record| include_revoked || record.is_active())
            .cloned()
            .collect();
        let project = match filter {
            PermissionProjectFilter::Current => Some(self.project_cwd()),
            PermissionProjectFilter::All => None,
            PermissionProjectFilter::Project(path) => Some(path.clone()),
        };
        if let Some(policy) = &self.policy {
            let mut state = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            inventory.extend(
                state
                    .state()?
                    .records()
                    .iter()
                    .filter(|record| {
                        (include_revoked || record.is_active())
                            && (project.is_none()
                                || record.project.is_none()
                                || record.project == project)
                    })
                    .cloned(),
            );
        }
        for record in &inventory {
            validate_compiled_templates(&record.rule)?;
        }
        inventory.sort_by_key(|record| record.created_at);
        Ok(inventory)
    }

    pub fn poll_permission_changes(&self) -> Result<bool, PermissionPolicyError> {
        let mut last = self
            .last_permission_poll
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if last.is_some_and(|last| last.elapsed() < PERMISSION_POLL_INTERVAL) {
            return Ok(false);
        }
        *last = Some(Instant::now());
        drop(last);
        self.refresh_permission_state()
    }

    pub fn refresh_permission_state(&self) -> Result<bool, PermissionPolicyError> {
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut changed = false;
        if let Some(policy) = &self.policy {
            let mut state = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let generation = state
                .state()?
                .generation()
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            let mut observed = self
                .external_generation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            changed = observed.as_ref() != Some(&generation);
            *observed = Some(generation);
        }
        if let Some(publication) = self.publication() {
            let snapshot = publication
                .snapshot()
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            let current = self
                .conversation_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if current.as_ref() != Some(&snapshot) {
                if let Err(error) = self.publish_conversation_snapshot(snapshot) {
                    *self
                        .conversation_policy_error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                    self.notify_policy_changed("");
                    return Err(PermissionPolicyError(error.to_string()));
                }
                changed = true;
            }
        }
        if changed {
            self.notify_policy_changed("");
        }
        Ok(changed)
    }

    pub fn revoke_structured_rule(
        &self,
        id: &str,
    ) -> Result<Option<RevokedRuleScope>, PermissionPolicyError> {
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_conversation_policy_valid()?;
        if let Some(publication) = self.publication() {
            let snapshot = publication
                .snapshot()
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            if snapshot
                .records
                .iter()
                .any(|record| record.id == id && record.is_active())
            {
                if snapshot.records != *self.structured_conversation_rules() {
                    return Err(PermissionPolicyError(
                        "conversation permission state changed".into(),
                    ));
                }
                let prepared = prepare_mutation(
                    vec![snapshot.clone()],
                    PermissionMutation::Revoke {
                        source: PermissionRecordIdentity {
                            owner: snapshot.revision.owner,
                            record_id: id.into(),
                        },
                    },
                )
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
                self.commit_prepared_permission_mutation(&prepared)
                    .map_err(|error| PermissionPolicyError(error.to_string()))?;
                self.notify_policy_changed("");
                return Ok(Some(RevokedRuleScope::Conversation));
            }
        }
        {
            let mut conversation = self.structured_conversation_rules();
            if let Some(record) = conversation
                .iter_mut()
                .find(|record| record.id == id && record.is_active())
            {
                if self.publication().is_some() {
                    return Err(PermissionPolicyError(
                        "conversation permission state changed".into(),
                    ));
                }
                record.revoked_at = Some(now_epoch().max(record.created_at));
                drop(conversation);
                self.notify_policy_changed("");
                return Ok(Some(RevokedRuleScope::Conversation));
            }
        }

        let Some(policy) = &self.policy else {
            return Ok(None);
        };
        let mut policy = policy.policy.lock().unwrap_or_else(|error| {
            warn!("permission policy mutex was poisoned, recovering");
            error.into_inner()
        });
        let state = policy.state()?;
        let snapshot = state
            .snapshot()
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        let Some(record) = snapshot
            .records
            .iter()
            .find(|record| record.id == id && record.is_active())
        else {
            return Ok(None);
        };
        let scope = match record.rule.lifetime {
            PermissionLifetime::Project => RevokedRuleScope::Project,
            PermissionLifetime::Global => RevokedRuleScope::Global,
            PermissionLifetime::Once | PermissionLifetime::Conversation => return Ok(None),
        };
        let prepared = prepare_mutation(
            vec![snapshot],
            PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: PermissionOwner::Persistent,
                    record_id: id.into(),
                },
            },
        )
        .map_err(|error| PermissionPolicyError(error.to_string()))?;
        drop(policy);
        self.commit_prepared_permission_mutation(&prepared)
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
        self.notify_policy_changed("");
        Ok(Some(scope))
    }
}

#[cfg(test)]
mod tests {

    use test_case::test_case;

    use crate::permissions::tests::{
        PLUGIN_EDIT_PATH, SHELL_WORKDIR, allows_without_prompt, answer_enforcement, default_mgr,
        denied_by_rule, deny_rule, enforce_shell_without_prompt, legacy_request, make_config,
        mgr_with, persistent_manager, plugin_edit_rule, remote_permission_asset, seeded_mgr,
        shell_request, workcell_shell_subject,
    };
    use crate::permissions::{
        PermissionAnswer, PermissionLifetime, PermissionManager, PermissionMode, PermissionRequest,
        PermissionRuleRecord, PluginRuleStore,
    };
    use caudra_config::{DefaultEffect, Effect, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[test_case(PermissionMode::Ask, None; "ask_seed")]
    #[test_case(PermissionMode::Auto, None; "auto_seed")]
    #[test_case(PermissionMode::Yolo, None; "yolo_seed")]
    #[test_case(PermissionMode::Auto, Some(PermissionMode::Ask); "stored_ask_overrides_auto")]
    #[test_case(PermissionMode::Yolo, Some(PermissionMode::Ask); "stored_ask_overrides_yolo")]
    #[test_case(PermissionMode::Yolo, Some(PermissionMode::Auto); "stored_auto_overrides_yolo")]
    #[test_case(PermissionMode::Auto, Some(PermissionMode::Yolo); "stored_yolo_overrides_auto")]
    fn session_mode_replaces_seed_and_forks_independently(
        seed: PermissionMode,
        stored: Option<PermissionMode>,
    ) {
        let manager = default_mgr();
        let revision = manager.broker.revision.load(Ordering::Acquire);
        manager.set_seed_mode(seed.clone());
        manager.set_session_mode(stored.clone());
        assert_eq!(manager.mode(), stored.clone().unwrap_or(seed.clone()));
        assert_eq!(manager.persisted_mode(), stored);
        assert!(manager.broker.revision.load(Ordering::Acquire) > revision);
        let fork = manager.fork();
        assert_eq!(fork.mode(), manager.mode());
        assert_eq!(fork.persisted_mode(), stored);
        fork.set_session_mode(None);
        assert_eq!(fork.mode(), seed);
        assert_eq!(fork.persisted_mode(), None);
        assert_eq!(manager.persisted_mode(), stored);
    }

    #[test_case(None; "seeded_auto")]
    #[test_case(Some(PermissionMode::Auto); "stored_auto")]
    fn unavailable_auto_acts_as_ask_and_keeps_its_intent(stored: Option<PermissionMode>) {
        let manager = mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"));
        manager.set_seed_mode(PermissionMode::Auto);
        manager.set_session_mode(stored.clone());
        assert_eq!(manager.mode(), PermissionMode::Ask);
        assert!(!manager.toggle_auto());
        assert_eq!(manager.persisted_mode(), stored);
        assert_eq!(manager.fork().mode(), PermissionMode::Ask);
        assert_eq!(manager.fork_session().mode(), PermissionMode::Ask);
    }

    #[test_case(PermissionMode::Ask; "from_ask")]
    #[test_case(PermissionMode::Auto; "from_auto")]
    #[test_case(PermissionMode::Yolo; "from_yolo")]
    fn auto_toggle_records_intent_and_is_exclusive(seed: PermissionMode) {
        let manager = default_mgr();
        manager.set_seed_mode(seed.clone());
        let enabled = manager.toggle_auto();
        assert_eq!(enabled, seed != PermissionMode::Auto);
        assert_eq!(
            manager.mode(),
            if enabled {
                PermissionMode::Auto
            } else {
                PermissionMode::Ask
            }
        );
        assert_eq!(manager.persisted_mode(), Some(manager.mode()));
        assert!(!manager.is_yolo());
        assert!(manager.toggle_yolo());
        assert_eq!(manager.mode(), PermissionMode::Yolo);
        assert!(manager.toggle_auto());
        assert_eq!(manager.mode(), PermissionMode::Auto);
        manager.set_seed_mode(PermissionMode::Yolo);
        assert_eq!(manager.mode(), PermissionMode::Auto);
    }

    #[test_case(PermissionMode::Ask; "ask_revision")]
    #[test_case(PermissionMode::Auto; "auto_revision")]
    #[test_case(PermissionMode::Yolo; "yolo_revision")]
    fn passive_decision_revision_rejects_mode_changes(mode: PermissionMode) {
        let manager = default_mgr();
        let revision = manager.passive_decision_revision().unwrap();
        assert!(manager.passive_decision_is_current(revision));
        manager.set_session_mode(Some(mode.clone()));
        assert!(!manager.passive_decision_is_current(revision));
        assert_eq!(
            manager.passive_decision_revision().is_none(),
            mode == PermissionMode::Yolo
        );
    }

    #[test]
    fn passive_decision_revision_rejects_service_replacement() {
        let manager = default_mgr();
        let revision = manager.passive_decision_revision().unwrap();
        manager.set_decisions(None);
        assert!(!manager.passive_decision_is_current(revision));
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

    #[test_case(false => (PermissionMode::Yolo, Some(PermissionMode::Yolo)); "toggling_on_claims_the_session")]
    #[test_case(true => (PermissionMode::Ask, Some(PermissionMode::Ask)); "toggling_off_under_the_flag_claims_the_session")]
    fn toggling_yolo_records_the_intent(seed: bool) -> (PermissionMode, Option<PermissionMode>) {
        let mgr = seeded_mgr(seed);
        assert_eq!(mgr.toggle_yolo(), !seed);
        (mgr.mode(), mgr.persisted_mode())
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
        for manager in [&mgr, &fork] {
            let request = legacy_request(manager, ToolKey::native("edit"), &[PLUGIN_EDIT_PATH]);
            assert!(allows_without_prompt(manager, &request));
        }
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

#[cfg(test)]
mod pattern_runtime_tests {
    use super::PermissionManager;
    use crate::permissions::{
        COMMAND_OBSERVATION_ATTRIBUTE, COMMAND_OBSERVATION_BINDING_ATTRIBUTE,
        COMMAND_TEMPLATE_PREFIX, ComposedAnswerError, ComposedRow, PermissionAnswer,
        PermissionAuthorityProfile, PermissionError, PermissionExecutorKind, PermissionLifetime,
        PermissionRequest, PermissionResource, PermissionResourceSelector, PermissionRowGrant,
        PermissionRuleOption, PermissionRuleRecord, PermissionSubject, ResourceCoverage,
        RuleOrigin, StructuredPermissionDecision, StructuredPermissionEffect,
        StructuredPermissionRule, evaluate_structured_permission_rules,
        pattern_recognition::{
            CommandObservation, PatternCandidate, PatternRecognizer, RecognizerLimits,
            ShellEffectStatus, fixtures,
        },
        permission_rule_covers_request, prepared_command_binding,
        tests::{
            SHELL_WORKDIR, default_mgr, make_config, persistent_manager, shell_intent,
            shell_policy_rule, workcell_shell_subject,
        },
    };
    use crate::permissions::{
        diagnostics::{
            PROMPT_MESSAGE_ASK, PROMPT_MESSAGE_FORCED, PROMPT_MESSAGE_PROTECTED,
            PROMPT_MESSAGE_REQUIRES_PROMPT, PROMPT_MESSAGE_UNCOVERED,
        },
        review::review_for_rule,
    };
    use crate::tools::{PermissionIntent, PermissionScopes};
    use crate::{AgentEvent, CancelToken, EventSender};
    use caudra_config::{DefaultEffect, Effect, PermissionsConfig, ToolKey};
    use caudra_storage::{
        StateDir,
        permission_patterns::{
            ArgumentDomain, ArgumentRole, OptionLikePolicy, PatternToken, SlotCombinations,
        },
        permission_state::PermissionState,
    };
    use futures_lite::future::poll_once;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use test_case::test_case;

    const PACKAGES: [&str; 3] = ["alpha", "beta", "gamma"];
    const NEW_PACKAGE: &str = "delta";
    const REQUEST_ID: &str = "runtime-template";
    const LATER_REQUEST_ID: &str = "later-template";
    const RENAMED_PATTERN: &str = "Renamed suggestion";
    const CARGO_CHECK: [&str; 2] = ["cargo", "check"];
    const JUST_TEST: [&str; 2] = ["just", "test"];
    const WORKDIR: &str = "workdir";
    const POSSIBLE_WORKDIRS: &str = "possible_workdirs";

    fn observation(id: &str, package: &str) -> CommandObservation {
        let mut observation =
            fixtures::observation(&["cargo", "check", "-p", package, "--tests"], id);
        observation.context.effective_workdir = SHELL_WORKDIR.into();
        observation.context.path_binding = SHELL_WORKDIR.into();
        observation.roles = vec![
            ArgumentRole::Executable,
            ArgumentRole::Operation,
            ArgumentRole::Flag,
            ArgumentRole::Data,
            ArgumentRole::Flag,
        ];
        observation
    }

    fn candidate() -> PatternCandidate {
        let mut recognizer =
            PatternRecognizer::new(RecognizerLimits::default(), fixtures::NOW_MS).unwrap();
        for package in PACKAGES {
            recognizer.observe(observation(package, package)).unwrap();
        }
        recognizer.suggestions().unwrap().remove(0)
    }

    fn request(id: &str, package: &str) -> PermissionRequest {
        request_for_project(id, package, Path::new(SHELL_WORKDIR))
    }

    fn candidate_for_project(project: &Path) -> PatternCandidate {
        let mut candidate = candidate();
        candidate.definition.context.path_binding = project.to_str().unwrap().into();
        candidate
    }

    fn attach_observation(
        resource: &mut PermissionResource,
        input: &Value,
        mut observation: CommandObservation,
    ) {
        let binding = prepared_command_binding(&resource.value, input);
        observation.source.input_hash = binding.clone();
        resource
            .attributes
            .insert(COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(), binding);
        resource.attributes.insert(
            COMMAND_OBSERVATION_ATTRIBUTE.into(),
            serde_json::to_string(&observation).unwrap(),
        );
    }

    fn request_for_project(id: &str, package: &str, project: &Path) -> PermissionRequest {
        request_from_observation(id, observation(id, package), project)
    }

    fn bare_request(id: &str, argv: &[&str], project: &Path) -> PermissionRequest {
        let mut fact = fixtures::observation(argv, id);
        fact.roles = vec![ArgumentRole::Executable, ArgumentRole::Operation];
        fact.context.effective_workdir = SHELL_WORKDIR.into();
        request_from_observation(id, fact, project)
    }

    fn request_from_observation(
        id: &str,
        mut fact: CommandObservation,
        project: &Path,
    ) -> PermissionRequest {
        let command = fact.argv.join(" ");
        let input = json!({"command": command, "workdir": SHELL_WORKDIR});
        let mut intent = shell_intent(&[&command]);
        fact.context.path_binding = project.to_str().unwrap().into();
        intent.resources[0].attributes.insert(
            POSSIBLE_WORKDIRS.into(),
            json!({"kind": "known", "symbolic_paths": [SHELL_WORKDIR]}).to_string(),
        );
        attach_observation(&mut intent.resources[0], &input, fact);
        PermissionRequest::from_intent_with_identity(
            id.into(),
            ToolKey::native("shell"),
            &intent,
            input,
            project,
            workcell_shell_subject(),
            PermissionExecutorKind::Native,
        )
    }

    fn offered_request() -> PermissionRequest {
        let mut request = request(REQUEST_ID, PACKAGES[0]);
        request.add_pattern_candidates(&vec![candidate()].into(), &[None]);
        request
    }

    fn is_learned_template(option: &PermissionRuleOption) -> bool {
        option.id.starts_with(COMMAND_TEMPLATE_PREFIX)
    }

    fn template_option_id(request: &PermissionRequest) -> String {
        request
            .options
            .iter()
            .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            .unwrap()
            .id
            .clone()
    }

    fn allows_without_prompt(manager: &PermissionManager, request: &PermissionRequest) -> bool {
        let rules = manager
            .applicable_rules_within(request, false, true)
            .unwrap();
        let coverage = manager.request_coverage(request, &rules, true);
        coverage.covered.iter().all(Option::is_some) && !coverage.must_prompt
    }

    fn template_rule(
        request: &PermissionRequest,
        lifetime: PermissionLifetime,
    ) -> StructuredPermissionRule {
        let option = request
            .options
            .iter()
            .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            .unwrap();
        request.option_rule(&option.id, lifetime).unwrap()
    }

    async fn enforce_prepared(
        manager: &PermissionManager,
        request: &PermissionRequest,
        events: &EventSender,
        forced: bool,
    ) -> Result<(), PermissionError> {
        let (_sender, receiver) = flume::unbounded();
        let responses = async_lock::Mutex::new(receiver);
        let intent = PermissionIntent::new(
            PermissionScopes {
                scopes: request.scopes.clone(),
                force_prompt: forced,
                plan_scoped: false,
            },
            request.resources.clone(),
            request.risk.clone(),
        )
        .with_authority(PermissionAuthorityProfile::Shell);
        manager
            .enforce_with_intent(
                &request.tool,
                &intent,
                &request.input,
                events,
                Some(&responses),
                &request.id,
                &CancelToken::none(),
                None,
                Some((request.subject.clone(), request.executor.clone())),
                true,
            )
            .await
    }

    async fn prompted(
        manager: &PermissionManager,
        request: &PermissionRequest,
        forced: bool,
        answer: impl FnOnce(&PermissionRequest) -> PermissionAnswer,
    ) -> PermissionRequest {
        let (events, received) = flume::unbounded();
        let events = EventSender::new(events, 0);
        let mut enforcement = Box::pin(enforce_prepared(manager, request, &events, forced));
        assert!(poll_once(&mut enforcement).await.is_none());
        let AgentEvent::PermissionRequest(offered) = received.try_recv().unwrap().event else {
            panic!("expected permission request")
        };
        assert!(manager.answer(&request.id, answer(&offered)));
        enforcement.await.unwrap();
        *offered
    }

    async fn automatic(manager: &PermissionManager, request: &PermissionRequest) {
        let (events, received) = flume::unbounded();
        let events = EventSender::new(events, 0);
        let mut enforcement = Box::pin(enforce_prepared(manager, request, &events, false));
        assert!(poll_once(&mut enforcement).await.unwrap().is_ok());
        assert!(received.try_recv().is_err());
    }

    #[test_case(&["nimblectl", "inspect"]; "arbitrary_cli")]
    #[test_case(&["forge", "verify"]; "another_arbitrary_cli")]
    #[test_case(&CARGO_CHECK; "cargo_check")]
    #[test_case(&JUST_TEST; "just_test")]
    fn runtime_templates_require_live_support_for_every_cli(argv: &[&str]) {
        smol::block_on(async {
            let manager = default_mgr();
            assert!(manager.pattern_proposal_inventory().2.is_empty());
            for count in 1..=super::RUNTIME_PATTERN_MIN_SUPPORT {
                let offered = prompted(
                    &manager,
                    &bare_request(
                        &format!("{REQUEST_ID}-{count}"),
                        argv,
                        Path::new(SHELL_WORKDIR),
                    ),
                    false,
                    |_| PermissionAnswer::AllowOnce,
                )
                .await;
                let expected = usize::from(count == super::RUNTIME_PATTERN_MIN_SUPPORT);
                assert_eq!(
                    offered
                        .options
                        .iter()
                        .filter(|option| is_learned_template(option))
                        .count(),
                    expected,
                );
                let candidates = manager.pattern_proposal_inventory().2;
                assert_eq!(candidates.len(), expected);
                if let Some(candidate) = candidates.first() {
                    assert_eq!(candidate.evidence.support.observations, count);
                    assert_eq!(candidate.evidence.support.independent_sessions, 1);
                }
                assert!(manager.structured_rule_inventory().unwrap().is_empty());
            }
        });
    }

    #[test_case(false; "automatic_default")]
    #[test_case(true; "broad_grant")]
    fn runtime_observes_covered_calls_and_suggests_without_granting(broad: bool) {
        smol::block_on(async {
            let manager = PermissionManager::new_nonpersistent(
                if broad {
                    make_config(vec![shell_policy_rule("*", Effect::Allow)])
                } else {
                    PermissionsConfig {
                        default: DefaultEffect::Allow,
                        ..PermissionsConfig::default()
                    }
                },
                PathBuf::from(SHELL_WORKDIR),
                Default::default(),
            );
            automatic(&manager, &request(PACKAGES[0], PACKAGES[0])).await;
            automatic(&manager, &request(PACKAGES[1], PACKAGES[1])).await;
            assert!(manager.pattern_candidates().proposals.is_empty());
            let offered = prompted(&manager, &request(PACKAGES[2], PACKAGES[2]), true, |_| {
                PermissionAnswer::AllowOnce
            })
            .await;
            assert_eq!(
                offered
                    .options
                    .iter()
                    .any(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX)),
                !broad,
            );
            assert_eq!(
                offered
                    .options
                    .iter()
                    .filter(|option| option.is_default)
                    .map(is_learned_template)
                    .collect::<Vec<_>>(),
                [!broad]
            );
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
            assert!(
                manager
                    .fork()
                    .structured_rule_inventory()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(manager.pattern_candidates().proposals.len(), 1);
        });
    }

    #[test]
    fn explicit_consent_persists_immutable_template_and_matches_after_reload() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let state = StateDir::from_path(temp.path().join("state"));
            let manager = persistent_manager(state.clone(), temp.path());
            manager.set_pattern_candidates(vec![candidate_for_project(temp.path())]);
            let offered = prompted(
                &manager,
                &request_for_project(REQUEST_ID, PACKAGES[0], temp.path()),
                false,
                |offered| PermissionAnswer::AllowOption {
                    option_id: offered
                        .options
                        .iter()
                        .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                        .unwrap()
                        .id
                        .clone(),
                    lifetime: PermissionLifetime::Project,
                },
            )
            .await;
            let before = manager.structured_rule_inventory().unwrap();
            assert_eq!(before.len(), 1);
            let reviewed = serde_json::to_string(&before[0]).unwrap();
            assert!(!reviewed.contains(COMMAND_OBSERVATION_ATTRIBUTE));
            assert!(
                before[0].review.as_ref().unwrap().resources[0]
                    .value
                    .as_ref()
                    .unwrap()
                    .contains("Command template")
            );
            let mut learned = candidate_for_project(temp.path());
            learned.definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
            learned.definition.combinations = SlotCombinations::Independent;
            manager.set_pattern_candidates(vec![learned]);
            manager.observe_pattern_request(&request_for_project(
                NEW_PACKAGE,
                NEW_PACKAGE,
                temp.path(),
            ));
            assert_eq!(manager.structured_rule_inventory().unwrap(), before);
            assert!(!allows_without_prompt(
                &manager,
                &request_for_project(NEW_PACKAGE, NEW_PACKAGE, temp.path())
            ));
            drop(manager);
            let reloaded = persistent_manager(state, temp.path());
            assert!(reloaded.pattern_candidates().proposals.is_empty());
            automatic(
                &reloaded,
                &request_for_project("later", PACKAGES[1], temp.path()),
            )
            .await;
            assert_eq!(reloaded.structured_rule_inventory().unwrap(), before);
            assert!(
                offered
                    .options
                    .iter()
                    .all(
                        |option| option.rule.resources.iter().all(|resource| !resource
                            .attributes
                            .contains_key(COMMAND_OBSERVATION_ATTRIBUTE))
                    )
            );
        });
    }

    #[test_case("extra_argument"; "extra_argument")]
    #[test_case("extra_flag"; "extra_flag")]
    #[test_case("environment"; "environment")]
    #[test_case("payload"; "payload")]
    #[test_case("unknown_effects"; "unknown_effects")]
    #[test_case("unverified"; "unverified")]
    #[test_case("missing"; "missing")]
    #[test_case("executable"; "executable")]
    #[test_case("binding"; "binding")]
    #[test_case("analysis"; "analysis")]
    #[test_case("tool_context"; "tool_context")]
    #[test_case("workdir"; "workdir")]
    #[test_case("owner"; "owner")]
    #[test_case("executor"; "executor")]
    #[test_case("tool"; "tool")]
    fn command_template_rejects_untrusted_or_changed_facts(change: &str) {
        let offered = offered_request();
        let rule = template_rule(&offered, PermissionLifetime::Conversation);
        let mut changed = request(REQUEST_ID, PACKAGES[0]);
        let mut fact = observation(REQUEST_ID, PACKAGES[0]);
        match change {
            "extra_argument" => {
                fact.argv.push(NEW_PACKAGE.into());
                fact.roles.push(ArgumentRole::Data);
            }
            "extra_flag" => {
                fact.argv.push("--fix".into());
                fact.roles.push(ArgumentRole::Flag);
            }
            "environment" => fact.verification.shell_effects = ShellEffectStatus::Present,
            "payload" => fact.roles[3] = ArgumentRole::Payload,
            "unknown_effects" => fact.verification.shell_effects = ShellEffectStatus::Unknown,
            "unverified" => fact.verification.context_verified = false,
            "missing" => {}
            "executable" => fact.context.executable_identity.push_str("-changed"),
            "binding" => fact.context.path_binding.push_str("-changed"),
            "analysis" => fact.context.analysis_version.push_str("-changed"),
            "tool_context" => fact.context.tool_identity.push_str("-changed"),
            "workdir" => {
                fact.context.effective_workdir = "/elsewhere".into();
                changed.resources[0]
                    .attributes
                    .insert(WORKDIR.into(), fact.context.effective_workdir.clone());
            }
            "owner" => {
                changed.subject = PermissionSubject::Native {
                    owner: "external".into(),
                    contract: "shell.execution.v1".into(),
                }
            }
            "executor" => changed.executor = PermissionExecutorKind::Lua,
            "tool" => changed.tool = ToolKey::native("not-shell"),
            _ => unreachable!(),
        }
        attach_observation(&mut changed.resources[0], &changed.input, fact);
        if change == "missing" {
            changed.resources[0]
                .attributes
                .remove(COMMAND_OBSERVATION_ATTRIBUTE);
        }
        assert!(!permission_rule_covers_request(&rule, &changed));
    }

    #[test]
    fn command_observation_never_survives_wire_or_forged_json() {
        let offered = offered_request();
        let rule = template_rule(&offered, PermissionLifetime::Conversation);
        let mut wire = serde_json::to_value(&offered).unwrap();
        assert!(
            wire["resources"][0]["attributes"]
                .get(COMMAND_OBSERVATION_ATTRIBUTE)
                .is_none()
        );
        wire["resources"][0]["attributes"][COMMAND_OBSERVATION_ATTRIBUTE] =
            json!(serde_json::to_string(&observation(REQUEST_ID, PACKAGES[0])).unwrap());
        let restored: PermissionRequest = serde_json::from_value(wire).unwrap();
        assert!(!permission_rule_covers_request(&rule, &restored));
        assert!(
            !restored.resources[0]
                .attributes
                .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
        );
    }

    #[test_case("executable"; "executable")]
    #[test_case("context"; "context")]
    #[test_case("role"; "role")]
    #[test_case("extra_row"; "extra_row")]
    #[test_case("extra_token"; "extra_token")]
    #[test_case("option_like"; "option_like")]
    fn edited_template_cannot_replace_current_offered_structure(change: &str) {
        let offered = offered_request();
        let mut definition = candidate().definition;
        let mut rows = vec![];
        match change {
            "executable" => {
                definition.argv[0] = PatternToken::Exact {
                    value: "other".into(),
                    role: ArgumentRole::Executable,
                }
            }
            "context" => definition.context.path_binding.push_str("-changed"),
            "role" => {
                definition.argv[3] = PatternToken::Slot {
                    id: definition.slots[0].id,
                    role: ArgumentRole::Unknown,
                }
            }
            "extra_row" => rows.push(None),
            "extra_token" => definition.argv.push(PatternToken::Exact {
                value: "--fix".into(),
                role: ArgumentRole::Flag,
            }),
            "option_like" => definition.slots[0].option_like = OptionLikePolicy::AllowForProvenData,
            _ => unreachable!(),
        }
        rows.push(Some(PermissionRowGrant::Pattern {
            option_id: template_option_id(&offered),
            definition: Box::new(definition),
        }));
        let rows = ComposedRow::uniform(rows, &PermissionLifetime::Conversation);
        assert!(offered.composed_rules(&rows).is_err());
        assert!(
            default_mgr()
                .commit_structured_decision(
                    &offered,
                    &PermissionAnswer::AllowComposed { rows },
                    None,
                    false,
                )
                .is_err()
        );
    }

    #[test_case("exact"; "exact")]
    #[test_case("set"; "set")]
    #[test_case("glob"; "glob")]
    #[test_case("regex"; "regex")]
    #[test_case("any"; "any")]
    fn user_can_edit_domain_names_and_combinations_without_changing_structure(domain: &str) {
        let offered = offered_request();
        let mut definition = candidate().definition;
        definition.name = "Reviewed checks".into();
        definition.slots[0].label = "package".into();
        definition.combinations = SlotCombinations::Independent;
        definition.slots[0].domain = match domain {
            "exact" => ArgumentDomain::Exact {
                value: PACKAGES[0].into(),
            },
            "set" => ArgumentDomain::ObservedSet {
                values: [PACKAGES[0].into(), NEW_PACKAGE.into()].into(),
            },
            "glob" => ArgumentDomain::Glob {
                pattern: "*a*".into(),
            },
            "regex" => ArgumentDomain::Regex {
                pattern: "alpha|delta".into(),
            },
            "any" => ArgumentDomain::AnyLiteralArgument,
            _ => unreachable!(),
        };
        let answer = PermissionAnswer::AllowComposed {
            rows: vec![Some(ComposedRow {
                grant: PermissionRowGrant::Pattern {
                    option_id: template_option_id(&offered),
                    definition: Box::new(definition.clone()),
                },
                lifetime: PermissionLifetime::Conversation,
            })],
        };
        assert_eq!(
            PermissionAnswer::decode(&answer.encode()),
            Some(answer.clone())
        );
        let manager = default_mgr();
        manager.set_pattern_candidates(vec![candidate()]);
        manager
            .commit_structured_decision(&offered, &answer, None, false)
            .unwrap();
        assert_eq!(
            manager.structured_rule_inventory().unwrap()[0]
                .rule
                .resources[0]
                .selector,
            PermissionResourceSelector::CommandTemplate {
                definition: Box::new(definition)
            }
        );
        assert!(allows_without_prompt(&manager, &offered));
        assert!(!allows_without_prompt(&manager.fork(), &offered));
    }

    #[test_case(StructuredPermissionEffect::Ask, StructuredPermissionDecision::Ask; "ask")]
    #[test_case(StructuredPermissionEffect::Deny, StructuredPermissionDecision::Deny; "deny")]
    fn template_never_outranks_overlapping_restrictions(
        effect: StructuredPermissionEffect,
        expected: StructuredPermissionDecision,
    ) {
        let offered = offered_request();
        let allow = template_rule(&offered, PermissionLifetime::Conversation);
        let mut restriction = allow.clone();
        restriction.resources[0].selector = PermissionResourceSelector::Any;
        restriction.effect = effect;
        assert_eq!(
            evaluate_structured_permission_rules(&[allow, restriction], &offered),
            expected
        );
    }

    #[test_case(PermissionLifetime::Conversation; "conversation")]
    #[test_case(PermissionLifetime::Project; "project")]
    fn invalid_regex_fails_closed_on_grant_and_reload_atomically(lifetime: PermissionLifetime) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let manager = persistent_manager(state.clone(), temp.path());
        let offered = offered_request();
        let valid = template_rule(&offered, lifetime.clone());
        let mut invalid = valid.clone();
        let PermissionResourceSelector::CommandTemplate { definition } =
            &mut invalid.resources[0].selector
        else {
            unreachable!()
        };
        definition.slots[0].domain = ArgumentDomain::Regex {
            pattern: "(".into(),
        };
        definition.combinations = SlotCombinations::Independent;
        assert!(
            manager
                .store_reusable_rules(
                    &offered,
                    vec![valid.clone(), invalid.clone()],
                    Some(temp.path()),
                    false,
                )
                .is_err()
        );
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
        manager
            .store_reusable_rules(&offered, vec![valid.clone()], Some(temp.path()), false)
            .unwrap();
        let approved = manager.structured_rule_inventory().unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].rule, valid);
        let reloaded = if lifetime == PermissionLifetime::Conversation {
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(invalid).unwrap(),
            ]);
            manager
        } else {
            assert_eq!(
                PermissionState::open(&state).unwrap().records(),
                approved.as_slice()
            );
            PermissionState::open(&state)
                .unwrap()
                .insert(Some(temp.path().to_path_buf()), invalid)
                .unwrap();
            assert!(manager.structured_rule_inventory().is_err());
            assert!(
                manager
                    .applicable_rules_within(&offered, false, true)
                    .is_err()
            );
            drop(manager);
            persistent_manager(state, temp.path())
        };
        assert!(reloaded.structured_rule_inventory().is_err());
        assert!(
            reloaded
                .applicable_rules_within(&offered, false, true)
                .is_err()
        );
    }

    #[test]
    fn candidates_cannot_shadow_exact_options_or_cross_rows_and_contexts() {
        let manager = default_mgr();
        let mut forged = candidate();
        forged.definition.argv[0] = PatternToken::Exact {
            value: "other".into(),
            role: ArgumentRole::Executable,
        };
        forged.definition.name = "allow_exact".into();
        manager.set_pattern_candidates(vec![forged]);
        let mut offered = request(REQUEST_ID, PACKAGES[0]);
        offered.add_pattern_candidates(&manager.pattern_candidates(), &[None]);
        assert!(!offered.options.iter().any(is_learned_template));
        manager.set_pattern_candidates(vec![candidate(); 100]);
        assert!(manager.pattern_candidates().proposals.len() <= super::MAX_RECOGNIZER_SUGGESTIONS);
        offered.add_pattern_candidates(
            &manager.pattern_candidates(),
            &[Some(ResourceCoverage {
                origin: RuleOrigin::Conversation,
                authority: "already allowed".into(),
                asks: false,
            })],
        );
        assert!(
            !offered
                .options
                .iter()
                .any(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        );
        offered.add_pattern_candidates(&manager.pattern_candidates(), &[None]);
        assert_eq!(
            offered
                .options
                .iter()
                .filter(|option| option.id == "allow_exact")
                .count(),
            1
        );
        assert_eq!(
            offered
                .options
                .iter()
                .filter(|option| is_learned_template(option))
                .count(),
            1
        );
        offered
            .options
            .retain(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX));
        let rows = ComposedRow::uniform(
            vec![Some(PermissionRowGrant::Pattern {
                option_id: template_option_id(&offered_request()),
                definition: Box::new(candidate().definition),
            })],
            &PermissionLifetime::Conversation,
        );
        assert_eq!(
            offered.composed_rules(&rows),
            Err(ComposedAnswerError::TemplateNotOffered)
        );
        let other_project = tempfile::tempdir().unwrap();
        manager.set_project(other_project.path());
        assert!(manager.pattern_candidates().proposals.is_empty());
    }

    #[test]
    fn mixed_command_scopes_offer_only_the_matching_resource_row() {
        let observed = request(REQUEST_ID, PACKAGES[1]);
        let mut intent = shell_intent(&["opaque program", &observed.resources[0].value]);
        intent.resources[0].protected = true;
        intent.resources[1] = observed.resources[0].clone();
        let input = json!({"command": "unchanged compound source"});
        attach_observation(
            &mut intent.resources[1],
            &input,
            observation(REQUEST_ID, PACKAGES[1]),
        );
        let mut mixed = PermissionRequest::from_intent_with_identity(
            REQUEST_ID.into(),
            ToolKey::native("shell"),
            &intent,
            input,
            &PathBuf::from(SHELL_WORKDIR),
            workcell_shell_subject(),
            PermissionExecutorKind::Native,
        );
        mixed.add_pattern_candidates(&vec![candidate()].into(), &[None, None]);
        let template = mixed
            .options
            .iter()
            .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            .unwrap();
        assert_eq!(template.group.as_ref().unwrap().resource, Some(1));
        let row = ComposedRow {
            grant: PermissionRowGrant::Pattern {
                option_id: template.id.clone(),
                definition: Box::new(candidate().definition),
            },
            lifetime: PermissionLifetime::Conversation,
        };
        assert!(mixed.composed_rules(&[Some(row.clone()), None]).is_err());
        let rules = mixed.composed_rules(&[None, Some(row)]).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(!permission_rule_covers_request(&rules[0], &mixed));
        assert!(permission_rule_covers_request(&rules[0], &observed));
    }

    #[test]
    fn template_answer_cannot_outlive_reviewed_project_context() {
        smol::block_on(async {
            let manager = default_mgr();
            manager.set_pattern_candidates(vec![candidate()]);
            let prepared = request(REQUEST_ID, PACKAGES[0]);
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_prepared(&manager, &prepared, &events, false));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(offered) = received.try_recv().unwrap().event else {
                panic!("expected permission request")
            };
            let option = offered
                .options
                .iter()
                .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                .unwrap();
            let next_project = tempfile::tempdir().unwrap();
            manager.set_project(next_project.path());
            manager.answer(
                REQUEST_ID,
                PermissionAnswer::AllowOption {
                    option_id: option.id.clone(),
                    lifetime: PermissionLifetime::Conversation,
                },
            );
            assert!(enforcement.await.is_err());
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
        });
    }

    #[test]
    fn prepared_workdir_fact_is_pinned_exactly_but_not_duplicated_in_template() {
        let offered = offered_request();
        let exact = offered
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        assert!(
            exact.resources[0]
                .attributes
                .contains_key(POSSIBLE_WORKDIRS)
        );
        assert!(
            !exact.resources[0]
                .attributes
                .contains_key(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
        );
        let template = template_rule(&offered, PermissionLifetime::Conversation);
        assert!(
            !template.resources[0]
                .attributes
                .contains_key(POSSIBLE_WORKDIRS)
        );
        assert!(permission_rule_covers_request(&exact, &offered));
        assert!(permission_rule_covers_request(&template, &offered));
        let review = review_for_rule(&offered, &exact);
        assert_eq!(
            review.resources[0].attributes[POSSIBLE_WORKDIRS],
            format!("Possible working directories: `{SHELL_WORKDIR}`")
        );
        assert_eq!(
            review.resources[0].attributes.len(),
            exact.resources[0].attributes.len()
        );
    }

    #[test]
    fn three_once_approved_calls_in_one_conversation_offer_only_a_proposal() {
        smol::block_on(async {
            let manager = default_mgr();
            for (index, package) in PACKAGES.iter().enumerate() {
                let prepared = request(package, package);
                let offered =
                    prompted(&manager, &prepared, false, |_| PermissionAnswer::AllowOnce).await;
                assert_eq!(
                    offered.options.iter().any(is_learned_template),
                    index == PACKAGES.len() - 1
                );
                assert_eq!(
                    offered
                        .options
                        .iter()
                        .filter(|option| option.is_default)
                        .map(is_learned_template)
                        .collect::<Vec<_>>(),
                    [index == PACKAGES.len() - 1]
                );
                assert!(manager.structured_rule_inventory().unwrap().is_empty());
            }
            let candidates = manager.pattern_candidates().proposals;
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].evidence.support.independent_sessions, 1);
            assert_eq!(candidates[0].evidence.support.observations, PACKAGES.len());
        });
    }

    #[test_case(r#"{"kind":"known","symbolic_paths":["/tmp","/elsewhere"]}"#; "multiple")]
    #[test_case(r#"{"kind":"known","symbolic_paths":["/elsewhere"]}"#; "mismatched")]
    #[test_case(r#"{"kind":"unknown"}"#; "unknown")]
    #[test_case(r#"{"kind":"known","symbolic_paths":[]}"#; "empty")]
    #[test_case(r#"{"kind":"known","symbolic_paths":["relative"]}"#; "relative")]
    #[test_case(r#"{"kind":"known","symbolic_paths":["/tmp"],"hidden":true}"#; "hidden")]
    fn command_template_requires_verified_singleton_possible_workdir(value: &str) {
        let original = offered_request();
        let template = template_rule(&original, PermissionLifetime::Conversation);
        let exact = original
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        let mut changed = original;
        changed.resources[0]
            .attributes
            .insert(POSSIBLE_WORKDIRS.into(), value.into());
        assert!(!permission_rule_covers_request(&template, &changed));
        assert!(!permission_rule_covers_request(&exact, &changed));
        changed.add_pattern_candidates(&vec![candidate()].into(), &[None]);
        assert!(
            changed
                .options
                .iter()
                .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        );
    }

    #[test_case("resource_source"; "resource_source")]
    #[test_case("command"; "command")]
    #[test_case("workdir"; "workdir")]
    #[test_case("extra_input"; "extra_input")]
    #[test_case("missing_binding"; "missing_binding")]
    #[test_case("swapped_fact"; "swapped_fact")]
    #[test_case("swapped_pair"; "swapped_pair")]
    fn prepared_binding_rejects_reused_native_facts(change: &str) {
        let mut request = offered_request();
        let rule = template_rule(&request, PermissionLifetime::Conversation);
        let other = self::request("other", PACKAGES[1]);
        match change {
            "resource_source" => request.resources[0].value.push_str(" --fix"),
            "command" => request.input["command"] = json!("cargo clean"),
            "workdir" => request.input["workdir"] = json!("/elsewhere"),
            "extra_input" => request.input["environment"] = json!({"INJECT": "value"}),
            "missing_binding" => {
                request.resources[0]
                    .attributes
                    .remove(COMMAND_OBSERVATION_BINDING_ATTRIBUTE);
            }
            "swapped_fact" | "swapped_pair" => {
                request.resources[0].attributes.insert(
                    COMMAND_OBSERVATION_ATTRIBUTE.into(),
                    other.resources[0].attributes[COMMAND_OBSERVATION_ATTRIBUTE].clone(),
                );
                if change == "swapped_pair" {
                    request.resources[0].attributes.insert(
                        COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(),
                        other.resources[0].attributes[COMMAND_OBSERVATION_BINDING_ATTRIBUTE]
                            .clone(),
                    );
                }
            }
            _ => unreachable!(),
        }
        assert!(!permission_rule_covers_request(&rule, &request));
    }

    #[test]
    fn discovery_is_project_scoped_shared_and_guarded_by_context_revision() {
        let manager = default_mgr();
        let (project, revision) = manager.pattern_candidate_context();
        manager
            .set_pattern_candidates_for_project(&project, revision, vec![candidate()])
            .unwrap();
        let fork = manager.fork();
        assert_eq!(
            fork.pattern_candidates().proposals,
            manager.pattern_candidates().proposals
        );
        let other = tempfile::tempdir().unwrap();
        fork.set_project(other.path());
        assert!(fork.pattern_candidates().proposals.is_empty());
        fork.set_pattern_candidates(vec![candidate_for_project(other.path())]);
        assert_eq!(manager.pattern_candidates().proposals, vec![candidate()]);
        assert_eq!(
            fork.pattern_candidates().proposals,
            vec![candidate_for_project(other.path())]
        );
        let error = fork
            .set_pattern_candidates_for_project(&project, revision, vec![candidate()])
            .unwrap_err();
        assert_eq!(error.0, super::STALE_PATTERN_CONTEXT);
        fork.set_pattern_candidates(vec![candidate()]);
        assert_eq!(
            fork.pattern_candidates().proposals,
            vec![candidate_for_project(other.path())]
        );
        manager.set_project(&project);
        let error = manager
            .set_pattern_candidates_for_project(&project, revision, vec![candidate()])
            .unwrap_err();
        assert_eq!(error.0, super::STALE_PATTERN_CONTEXT);
    }

    #[test_case(false; "dismiss")]
    #[test_case(true; "snooze")]
    fn proposal_actions_reject_stale_project_revision_and_definition(snooze: bool) {
        let manager = default_mgr();
        manager.set_pattern_candidates(vec![candidate()]);
        let (project, revision, proposals) = manager.pattern_proposal_inventory();
        let id = proposals[0].definition.fingerprint().unwrap();
        let other = tempfile::tempdir().unwrap();
        manager.set_project(other.path());
        let error = manager
            .hide_pattern_proposal_at(&project, revision, &id, snooze, fixtures::NOW_MS / 1000)
            .unwrap_err();
        assert_eq!(error.0, super::STALE_PATTERN_CONTEXT);
        manager.set_project(&project);
        let error = manager
            .hide_pattern_proposal_at(&project, revision, &id, snooze, fixtures::NOW_MS / 1000)
            .unwrap_err();
        assert_eq!(error.0, super::STALE_PATTERN_CONTEXT);
        let (_, revision) = manager.pattern_candidate_context();
        let mut changed = candidate();
        changed.definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
        changed.definition.combinations = SlotCombinations::Independent;
        manager.set_pattern_candidates(vec![changed.clone()]);
        let error = manager
            .hide_pattern_proposal_at(&project, revision, &id, snooze, fixtures::NOW_MS / 1000)
            .unwrap_err();
        assert_eq!(error.0, super::STALE_PATTERN_PROPOSAL);
        assert_eq!(manager.pattern_proposal_inventory().2, vec![changed]);
        assert!(
            manager
                .pattern_dismissals
                .lock()
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn dismissed_proposals_require_material_change_and_leave_grants_intact() {
        let manager = default_mgr();
        let rule = request(REQUEST_ID, NEW_PACKAGE)
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        let grant = PermissionRuleRecord::conversation(rule).unwrap();
        manager.load_structured_conversation_rules(vec![grant.clone()]);
        manager.set_pattern_candidates(vec![candidate()]);
        let (project, revision, proposals) = manager.pattern_proposal_inventory();
        let id = proposals[0].definition.fingerprint().unwrap();
        manager
            .dismiss_pattern_proposal(&project, revision, &id)
            .unwrap();
        assert!(manager.pattern_proposal_inventory().2.is_empty());
        assert!(manager.fork().pattern_candidates().proposals.is_empty());
        let mut renamed = candidate();
        renamed.definition.name = RENAMED_PATTERN.into();
        renamed.definition.slots[0].label = "renamed".into();
        renamed.evidence.support.observations += 1;
        manager.set_pattern_candidates(vec![renamed.clone()]);
        assert!(manager.pattern_candidates().proposals.is_empty());
        renamed.definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
        renamed.definition.combinations = SlotCombinations::Independent;
        manager.set_pattern_candidates(vec![renamed.clone()]);
        assert_eq!(manager.pattern_candidates().proposals, vec![renamed]);
        assert_eq!(manager.structured_rule_inventory().unwrap(), vec![grant]);
    }

    #[test]
    fn dismissed_candidates_do_not_consume_the_visible_inventory_limit() {
        let manager = default_mgr();
        let learned = candidate();
        manager.set_pattern_candidates(vec![learned.clone()]);
        let (project, revision, _) = manager.pattern_proposal_inventory();
        manager
            .dismiss_pattern_proposal(
                &project,
                revision,
                &learned.definition.fingerprint().unwrap(),
            )
            .unwrap();
        manager
            .patterns
            .lock()
            .unwrap()
            .get_mut(&project)
            .unwrap()
            .learned = vec![learned; super::MAX_RECOGNIZER_SUGGESTIONS];
        let mut other = candidate();
        other.definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
        other.definition.combinations = SlotCombinations::Independent;
        manager.set_pattern_candidates(vec![other.clone(); super::MAX_RECOGNIZER_SUGGESTIONS]);
        assert_eq!(manager.pattern_proposal_inventory().2, vec![other]);
    }

    #[test_case(&CARGO_CHECK; "cargo_check")]
    #[test_case(&JUST_TEST; "just_test")]
    fn snoozed_proposals_expire_at_the_injected_clock_boundary(argv: &[&str]) {
        let manager = default_mgr();
        for index in 0..super::RUNTIME_PATTERN_MIN_SUPPORT {
            manager.observe_pattern_request(&bare_request(
                &format!("{REQUEST_ID}-{index}"),
                argv,
                Path::new(SHELL_WORKDIR),
            ));
        }
        let (project, revision, proposals) = manager.pattern_proposal_inventory();
        let id = proposals[0].definition.fingerprint().unwrap();
        let now = fixtures::NOW_MS / 1000;
        manager
            .hide_pattern_proposal_at(&project, revision, &id, true, now)
            .unwrap();
        let expiry = now + super::PATTERN_SNOOZE_SECONDS;
        let mut offered = bare_request(REQUEST_ID, argv, &project);
        let hidden = manager.pattern_candidates_at(&project, expiry - 1);
        assert!(hidden.proposals.is_empty());
        offered.add_pattern_candidates(&hidden, &[None]);
        assert!(
            offered
                .options
                .iter()
                .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
        );
        let restored = manager.pattern_candidates_at(&project, expiry);
        assert_eq!(restored.proposals, proposals);
        offered.add_pattern_candidates(&restored, &[None]);
        assert!(offered.options.iter().any(is_learned_template));
        assert!(
            manager
                .pattern_dismissals
                .lock()
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test_case(false; "dismiss")]
    #[test_case(true; "snooze")]
    fn dismissal_settings_persist_only_bounded_digests_and_are_project_bound(snooze: bool) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let manager = persistent_manager(state.clone(), temp.path());
        let learned = candidate_for_project(temp.path());
        manager.set_pattern_candidates(vec![learned.clone()]);
        let (project, revision, proposals) = manager.pattern_proposal_inventory();
        let id = proposals[0].definition.fingerprint().unwrap();
        let now = super::now_epoch();
        manager
            .hide_pattern_proposal_at(&project, revision, &id, snooze, now)
            .unwrap();
        let settings = super::PatternDismissals::load(&state);
        assert_eq!(settings.entries.len(), 1);
        assert_eq!(
            settings.entries[0].expires_at,
            snooze.then_some(now + super::PATTERN_SNOOZE_SECONDS)
        );
        assert_eq!(
            settings.entries[0].project,
            super::pattern_project_digest(&project)
        );
        let encoded = serde_json::to_string(&settings).unwrap();
        assert!(!encoded.contains(project.to_str().unwrap()));
        assert!(!encoded.contains(&learned.definition.name));
        assert!(!encoded.contains(PACKAGES[0]));
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
        let reloaded = persistent_manager(state, temp.path());
        reloaded.set_pattern_candidates(vec![learned]);
        assert!(reloaded.pattern_candidates().proposals.is_empty());
        let other = tempfile::tempdir().unwrap();
        reloaded.set_project(other.path());
        reloaded.set_pattern_candidates(vec![candidate_for_project(other.path())]);
        assert_eq!(reloaded.pattern_candidates().proposals.len(), 1);
    }

    #[test]
    fn dismissal_settings_prune_expired_invalid_and_oldest_entries() {
        let now = fixtures::NOW_MS / 1000;
        let mut settings = super::PatternDismissals::default();
        for index in 0..=super::MAX_PATTERN_DISMISSALS {
            settings.insert(
                super::PatternDismissal {
                    project: super::pattern_project_digest(Path::new(&index.to_string())),
                    definition: [0; super::DIGEST_BYTES],
                    expires_at: None,
                },
                now,
            );
        }
        assert_eq!(settings.entries.len(), super::MAX_PATTERN_DISMISSALS);
        assert_eq!(
            settings.entries[0].project,
            super::pattern_project_digest(Path::new("1"))
        );
        settings.entries[0].expires_at = Some(now);
        settings.entries[1].expires_at = Some(u64::MAX);
        settings.prune(now);
        assert_eq!(settings.entries.len(), super::MAX_PATTERN_DISMISSALS - 2);
        let invalid = json!({"entries": [{"project": "personal path", "definition": "command", "expires_at": null}]});
        assert!(serde_json::from_value::<super::PatternDismissals>(invalid).is_err());
    }

    #[test_case(&CARGO_CHECK, false; "dismiss_cargo_check")]
    #[test_case(&CARGO_CHECK, true; "snooze_cargo_check")]
    #[test_case(&JUST_TEST, false; "dismiss_just_test")]
    #[test_case(&JUST_TEST, true; "snooze_just_test")]
    fn dismissal_refreshes_pending_proposals_and_rejects_stale_approval(
        argv: &[&str],
        snooze: bool,
    ) {
        smol::block_on(async {
            let manager = default_mgr();
            for index in 0..super::RUNTIME_PATTERN_MIN_SUPPORT {
                prompted(
                    &manager,
                    &bare_request(
                        &format!("{REQUEST_ID}-{index}"),
                        argv,
                        Path::new(SHELL_WORKDIR),
                    ),
                    false,
                    |_| PermissionAnswer::AllowOnce,
                )
                .await;
            }
            let prepared = bare_request(REQUEST_ID, argv, Path::new(SHELL_WORKDIR));
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_prepared(&manager, &prepared, &events, false));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(initial) = received.try_recv().unwrap().event else {
                panic!("expected permission request")
            };
            let stale_id = template_option_id(&initial);
            let (project, revision, proposals) = manager.pattern_proposal_inventory();
            assert_eq!(proposals.len(), 1);
            assert_eq!(
                proposals[0].evidence.support.observations,
                super::RUNTIME_PATTERN_MIN_SUPPORT + 1
            );
            let definition_id = proposals[0].definition.fingerprint().unwrap();
            assert!(initial.options.iter().any(is_learned_template));
            assert_eq!(
                initial
                    .options
                    .iter()
                    .filter(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                    .count(),
                1
            );
            let fork = manager.fork();
            if snooze {
                fork.snooze_pattern_proposal(&project, revision, &definition_id)
            } else {
                fork.dismiss_pattern_proposal(&project, revision, &definition_id)
            }
            .unwrap();
            assert!(manager.pattern_proposal_inventory().2.is_empty());
            assert!(!manager.answer(
                REQUEST_ID,
                PermissionAnswer::AllowOption {
                    option_id: stale_id,
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequestUpdated(updated) = received.try_recv().unwrap().event
            else {
                panic!("expected permission update")
            };
            assert!(
                updated
                    .options
                    .iter()
                    .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            );
            let offered_rungs = |options: &[PermissionRuleOption]| {
                options
                    .iter()
                    .filter(|option| !is_learned_template(option))
                    .map(|option| PermissionRuleOption {
                        is_default: false,
                        ..option.clone()
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                offered_rungs(&updated.options),
                offered_rungs(&initial.options)
            );
            assert_eq!(
                updated
                    .options
                    .iter()
                    .filter(|option| option.is_default)
                    .count(),
                1,
                "the withdrawn suggestion's row falls back to another rung"
            );
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
            assert!(manager.answer(REQUEST_ID, PermissionAnswer::AllowOnce));
            enforcement.await.unwrap();

            let mut renamed = proposals[0].clone();
            renamed.definition.name = RENAMED_PATTERN.into();
            assert_eq!(renamed.definition.fingerprint().unwrap(), definition_id);
            manager.set_pattern_candidates(vec![renamed]);
            let later = prompted(
                &manager,
                &bare_request(LATER_REQUEST_ID, argv, &project),
                false,
                |_| PermissionAnswer::AllowOnce,
            )
            .await;
            assert!(
                later
                    .options
                    .iter()
                    .all(|option| !option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
            );
            assert!(manager.pattern_proposal_inventory().2.is_empty());
            assert!(manager.structured_rule_inventory().unwrap().is_empty());

            let other_project = tempfile::tempdir().unwrap();
            fork.set_project(other_project.path());
            for count in 1..=super::RUNTIME_PATTERN_MIN_SUPPORT {
                let other = prompted(
                    &fork,
                    &bare_request(
                        &format!("{LATER_REQUEST_ID}-{count}"),
                        argv,
                        other_project.path(),
                    ),
                    false,
                    |_| PermissionAnswer::AllowOnce,
                )
                .await;
                assert_eq!(
                    other.options.iter().any(is_learned_template),
                    count == super::RUNTIME_PATTERN_MIN_SUPPORT,
                );
            }
            assert_eq!(fork.pattern_proposal_inventory().2.len(), 1);
            assert!(fork.structured_rule_inventory().unwrap().is_empty());
            assert!(manager.pattern_proposal_inventory().2.is_empty());
        });
    }

    #[test]
    fn pending_candidates_refresh_and_stale_answers_never_write_authority() {
        smol::block_on(async {
            let manager = default_mgr();
            let fork = manager.fork();
            let prepared = request(REQUEST_ID, PACKAGES[0]);
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_prepared(&manager, &prepared, &events, false));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(initial) = received.try_recv().unwrap().event else {
                panic!("expected permission request")
            };
            assert!(
                initial
                    .options
                    .iter()
                    .all(|option| !is_learned_template(option))
            );
            fork.set_pattern_candidates(vec![candidate()]);
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequestUpdated(first) = received.try_recv().unwrap().event
            else {
                panic!("expected permission update")
            };
            let stale_id = template_option_id(&first);
            let mut wider = candidate();
            wider.definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
            wider.definition.combinations = SlotCombinations::Independent;
            fork.set_pattern_candidates(vec![wider.clone()]);
            let stale_answer = PermissionAnswer::AllowOption {
                option_id: stale_id.clone(),
                lifetime: PermissionLifetime::Conversation,
            };
            assert!(!manager.answer(REQUEST_ID, stale_answer.clone()));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequestUpdated(second) = received.try_recv().unwrap().event
            else {
                panic!("expected permission update")
            };
            assert_ne!(template_option_id(&second), stale_id);
            assert!(!manager.answer(REQUEST_ID, stale_answer));
            assert!(!manager.answer(
                REQUEST_ID,
                PermissionAnswer::AllowComposed {
                    rows: vec![Some(ComposedRow {
                        grant: PermissionRowGrant::Pattern {
                            option_id: stale_id,
                            definition: Box::new(candidate().definition)
                        },
                        lifetime: PermissionLifetime::Conversation
                    })],
                }
            ));
            let revision = manager.broker.revision.load(Ordering::Acquire);
            fork.set_pattern_candidates(vec![wider]);
            assert_eq!(manager.broker.revision.load(Ordering::Acquire), revision);
            assert!(poll_once(&mut enforcement).await.is_none());
            assert!(received.try_recv().is_err());
            fork.set_pattern_candidates(Vec::new());
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequestUpdated(cleared) = received.try_recv().unwrap().event
            else {
                panic!("expected permission update")
            };
            assert!(
                cleared
                    .options
                    .iter()
                    .all(|option| !is_learned_template(option))
            );
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
            assert!(manager.answer(REQUEST_ID, PermissionAnswer::AllowOnce));
            enforcement.await.unwrap();
            assert_eq!(manager.pending_count(), 0);
        });
    }

    #[test_case("uncovered", PROMPT_MESSAGE_UNCOVERED; "uncovered")]
    #[test_case("forced", PROMPT_MESSAGE_FORCED; "forced")]
    #[test_case("protected", PROMPT_MESSAGE_PROTECTED; "protected")]
    #[test_case("prepared", PROMPT_MESSAGE_REQUIRES_PROMPT; "prepared")]
    #[test_case("ask", PROMPT_MESSAGE_ASK; "ask")]
    fn prompt_presentation_explains_current_policy(reason: &str, expected: &str) {
        smol::block_on(async {
            let manager = if reason == "ask" {
                PermissionManager::new_nonpersistent(
                    make_config(vec![shell_policy_rule("cargo *", Effect::Ask)]),
                    PathBuf::from(SHELL_WORKDIR),
                    Default::default(),
                )
            } else {
                default_mgr()
            };
            let mut prepared = request(REQUEST_ID, PACKAGES[0]);
            prepared.resources[0].protected = reason == "protected";
            prepared.resources[0].requires_prompt = reason == "prepared";
            let offered = prompted(&manager, &prepared, reason == "forced", |_| {
                PermissionAnswer::AllowOnce
            })
            .await;
            assert_eq!(offered.presentation.risk_summary, expected);
        });
    }

    #[test]
    fn pending_prompt_refreshes_policy_reason_without_coverage_change() {
        smol::block_on(async {
            let manager = default_mgr();
            let prepared = request(REQUEST_ID, PACKAGES[0]);
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_prepared(&manager, &prepared, &events, false));
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(initial) = received.try_recv().unwrap().event else {
                panic!("expected permission request")
            };
            assert_eq!(initial.presentation.risk_summary, PROMPT_MESSAGE_UNCOVERED);
            manager
                .plugin_rules
                .replace("ask", vec![shell_policy_rule("cargo *", Effect::Ask)]);
            assert!(poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequestUpdated(updated) = received.try_recv().unwrap().event
            else {
                panic!("expected permission update")
            };
            assert_eq!(
                updated.presentation.resources,
                initial.presentation.resources
            );
            assert_eq!(updated.presentation.risk_summary, PROMPT_MESSAGE_ASK);
            assert!(manager.answer(REQUEST_ID, PermissionAnswer::AllowOnce));
            enforcement.await.unwrap();
        });
    }
}
