//! Persisted model tier assignments and workload-role selections.
//!
//! Three layers, checked in order: user overrides (persisted, one model per
//! tier) > static entries from the provider registry > auto-assignment by
//! position in `list_models()`.
//!
//! Discovered metadata (context windows, pricing) from `/models` endpoints is
//! stored in `known_models` and consulted by [`crate::model::Model::from_base`].
//!
//! The global lock never escapes this module: accessors lock internally and
//! return owned data, so a caller can never hold a read guard across model
//! construction (recursive read + queued writer = deadlock). The module owns
//! persistence: [`load_from_storage`] at startup and typed setters on user edits.
//! Callers never touch the stored representation directly.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use caudra_storage::state::{self, SCOPE_GLOBAL, StateKey};
use caudra_storage::{StateClass, StateDir};
use tracing::warn;

use crate::manifest::ManifestRegistry;
use crate::model::{ModelInfo, ModelTier};

const TIERS: StateKey = StateKey {
    name: "model.tiers",
    class: StateClass::Persistent,
};
const ROLES: StateKey = StateKey {
    name: "model.roles",
    class: StateClass::Persistent,
};

static REGISTRY: OnceLock<RwLock<ModelRegistry>> = OnceLock::new();

fn read() -> RwLockReadGuard<'static, ModelRegistry> {
    registry().read().unwrap()
}

fn write() -> RwLockWriteGuard<'static, ModelRegistry> {
    registry().write().unwrap()
}

fn registry() -> &'static RwLock<ModelRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ModelRegistry::default()))
}

pub fn spec_for_tier(provider: &str, tier: ModelTier) -> Option<String> {
    read().spec_for_tier(provider, tier)
}

pub fn override_spec_for_tier(tier: ModelTier) -> Option<String> {
    read().overrides.get(&tier).cloned()
}

pub fn discovered(provider: &str, model_id: &str) -> Option<ModelInfo> {
    read().discovered(provider, model_id).cloned()
}

/// Typed `provider_info` for a discovered model. Key by the builtin slug, not
/// `model.provider`: a dynamic wrap's model carries its own slug.
pub fn provider_info<T: Send + Sync + 'static>(provider: &str, model_id: &str) -> Option<Arc<T>> {
    let info = read()
        .discovered(provider, model_id)?
        .provider_info
        .clone()?;
    Arc::downcast(info).ok()
}

pub fn tier_for(spec: &str, provider: &str, static_tier: Option<ModelTier>) -> ModelTier {
    read().tier_for(spec, provider, static_tier)
}

pub fn set_known_models(provider: &str, models: Vec<ModelInfo>) {
    write().set_known_models(provider, models);
}

/// Tiers whose override points at `spec`, in descending tier order.
pub fn override_tiers(spec: &str) -> Vec<ModelTier> {
    read().override_tiers(spec)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum GoalEvaluatorTarget {
    #[default]
    Auto,
    Tier(ModelTier),
    Model(String),
}

impl fmt::Display for GoalEvaluatorTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("default"),
            Self::Tier(ModelTier::Weak) => f.write_str("fast"),
            Self::Tier(ModelTier::Medium) => f.write_str("balanced"),
            Self::Tier(ModelTier::Strong) => f.write_str("best"),
            Self::Model(spec) => f.write_str(spec),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CompactionTarget {
    #[default]
    Auto,
    Model(String),
}

impl fmt::Display for CompactionTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("default"),
            Self::Model(spec) => f.write_str(spec),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TitleTarget {
    #[default]
    Auto,
    Model(String),
}

impl fmt::Display for TitleTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("default"),
            Self::Model(spec) => f.write_str(spec),
        }
    }
}

pub fn goal_evaluator_target() -> GoalEvaluatorTarget {
    read().goal_evaluator.clone().unwrap_or_default()
}

pub fn compaction_target() -> CompactionTarget {
    read().compaction.clone().unwrap_or_default()
}

pub fn title_target() -> TitleTarget {
    read().title.clone().unwrap_or_default()
}

pub fn load_from_storage(dir: &StateDir) {
    let overrides = read_overrides(dir);
    let PersistedRoles {
        goal_evaluator,
        compaction,
        title,
    } = read_roles(dir);
    let mut registry = write();
    registry.set_overrides(overrides);
    registry.goal_evaluator = goal_evaluator.filter(|target| *target != GoalEvaluatorTarget::Auto);
    registry.compaction = compaction;
    registry.title = title;
}

pub fn set_and_persist(spec: String, tier: ModelTier, dir: &StateDir) {
    update_and_persist(dir, move |overrides| {
        overrides.insert(tier, spec.clone());
    });
}

pub fn unset_and_persist(spec: &str, tier: ModelTier, dir: &StateDir) {
    update_and_persist(dir, |overrides| {
        if overrides.get(&tier).map(String::as_str) == Some(spec) {
            overrides.remove(&tier);
        }
    });
}

pub fn reset_tier_and_persist(tier: ModelTier, dir: &StateDir) {
    update_and_persist(dir, |overrides| {
        overrides.remove(&tier);
    });
}

pub fn set_goal_evaluator_and_persist(target: GoalEvaluatorTarget, dir: &StateDir) {
    let persisted = (target != GoalEvaluatorTarget::Auto).then_some(target.clone());
    {
        let mut registry = write();
        registry.goal_evaluator = (target != GoalEvaluatorTarget::Auto).then_some(target);
    }
    update_persisted_roles(dir, |roles| roles.goal_evaluator = persisted);
}

pub fn set_compaction_and_persist(target: CompactionTarget, dir: &StateDir) {
    {
        let mut registry = write();
        registry.compaction = Some(target.clone());
    }
    update_persisted_roles(dir, |roles| roles.compaction = Some(target));
}

pub fn set_title_model_and_persist(target: TitleTarget, dir: &StateDir) {
    {
        let mut registry = write();
        registry.title = Some(target.clone());
    }
    update_persisted_roles(dir, |roles| roles.title = Some(target));
}

fn update_and_persist(dir: &StateDir, update: impl Fn(&mut BTreeMap<ModelTier, String>)) {
    {
        let mut reg = write();
        update(&mut reg.overrides);
    }
    update_persisted_overrides(dir, update);
}

fn update_persisted_overrides(
    dir: &StateDir,
    update: impl FnOnce(&mut BTreeMap<ModelTier, String>),
) {
    if let Err(error) = state::update(dir, SCOPE_GLOBAL, TIERS, update) {
        warn!(%error, "failed to persist tier overrides");
    }
}

#[derive(Debug, Default)]
struct ModelRegistry {
    /// Keyed by tier (not spec) so inserting a model automatically evicts the
    /// previous holder. Persisted to disk.
    overrides: BTreeMap<ModelTier, String>,
    /// Ordered model info per provider, populated from `list_models()`.
    /// Not persisted - rebuilt every session. Used for auto-tier assignment
    /// and discovered metadata lookup.
    known_models: HashMap<String, Vec<ModelInfo>>,
    goal_evaluator: Option<GoalEvaluatorTarget>,
    compaction: Option<CompactionTarget>,
    title: Option<TitleTarget>,
}

impl ModelRegistry {
    fn set_overrides(&mut self, overrides: BTreeMap<ModelTier, String>) {
        self.overrides = overrides;
    }

    fn set_known_models(&mut self, provider: &str, models: Vec<ModelInfo>) {
        self.known_models.insert(provider.to_string(), models);
    }

    #[cfg(test)]
    fn set(&mut self, spec: String, tier: ModelTier) {
        self.overrides.insert(tier, spec);
    }

    #[cfg(test)]
    fn unset(&mut self, spec: &str, tier: ModelTier) {
        if self.has_override(spec, tier) {
            self.overrides.remove(&tier);
        }
    }

    #[cfg(test)]
    fn has_override(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.get(&tier).map(String::as_str) == Some(spec)
    }

    /// Lookup discovered metadata for a model by ID.
    fn discovered(&self, provider: &str, model_id: &str) -> Option<&ModelInfo> {
        self.known_models
            .get(provider)?
            .iter()
            .find(|m| m.id == model_id)
    }

    fn tier_for(&self, spec: &str, provider: &str, static_tier: Option<ModelTier>) -> ModelTier {
        // A spec may hold several presets; prefer the strongest assignment.
        if let Some(first) = self.override_tiers(spec).into_iter().next() {
            return first;
        }
        if tiers_from_discovery(provider)
            && let Some((_, model_id)) = spec.split_once('/')
            && let Some(models) = self.known_models.get(provider)
            && let Some(pos) = models.iter().position(|model| model.id == model_id)
        {
            if let Some(tier) = models[pos].tier {
                return tier;
            }
            if static_tier.is_none() {
                return tier_for_position(pos);
            }
        }
        if let Some(t) = static_tier {
            return t;
        }
        ModelTier::Medium
    }

    fn spec_for_tier(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let prefix = format!("{provider}/");
        if let Some(spec) = self.overrides.get(&tier)
            && spec.starts_with(&prefix)
        {
            return Some(spec.clone());
        }

        let candidate = if tiers_from_discovery(provider) {
            self.discovered_static_candidate(provider, tier)
                .or_else(|| self.metadata_candidate(provider, tier))
                .or_else(|| static_candidate(provider, tier))
                .or_else(|| self.positional_candidate(provider, tier))
        } else {
            static_candidate(provider, tier)
        }?;

        (!self.claimed_elsewhere(&candidate, tier)).then_some(candidate)
    }

    /// The curated default the provider actually offers, so discovery cannot
    /// replace a still available default with whatever it lists first.
    fn discovered_static_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        static_prefixes(provider, tier)
            .find(|prefix| self.discovered(provider, prefix).is_some())
            .map(|prefix| format!("{provider}/{prefix}"))
    }

    /// Lowest ID wins, so the tier default survives provider list reordering.
    fn metadata_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        self.known_models
            .get(provider)?
            .iter()
            .filter(|model| model.tier == Some(tier))
            .map(|model| model.id.as_str())
            .min()
            .map(|id| format!("{provider}/{id}"))
    }

    fn positional_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let models = self.known_models.get(provider).filter(|m| !m.is_empty())?;
        let slot = match tier {
            ModelTier::Strong => 0,
            ModelTier::Medium => 1,
            ModelTier::Weak => 2,
        };
        Some(format!(
            "{provider}/{}",
            models[slot.min(models.len() - 1)].id
        ))
    }

    fn claimed_elsewhere(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.iter().any(|(&t, s)| s == spec && t != tier)
    }

    fn override_tiers(&self, spec: &str) -> Vec<ModelTier> {
        self.overrides
            .iter()
            .rev()
            .filter(|(_, s)| s.as_str() == spec)
            .map(|(&t, _)| t)
            .collect()
    }
}

/// Discovery metadata (context window, pricing, vision) is stored for every
/// provider, but only providers that accept arbitrary models may use the
/// discovered list for tier auto-assignment; curated providers keep their
/// static tier tables.
fn tiers_from_discovery(provider: &str) -> bool {
    ManifestRegistry::get(provider).is_none_or(|m| m.accepts_arbitrary_models)
}

fn static_candidate(provider: &str, tier: ModelTier) -> Option<String> {
    static_prefixes(provider, tier)
        .next()
        .map(|prefix| format!("{provider}/{prefix}"))
}

fn static_prefixes(provider: &str, tier: ModelTier) -> impl Iterator<Item = &'static str> {
    ManifestRegistry::get(provider)
        .into_iter()
        .flat_map(|manifest| manifest.models)
        .filter(move |entry| entry.default && entry.tier == tier)
        .flat_map(|entry| entry.prefixes.iter().copied())
}

fn tier_for_position(pos: usize) -> ModelTier {
    [ModelTier::Strong, ModelTier::Medium, ModelTier::Weak][pos.min(2)]
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedRoles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    goal_evaluator: Option<GoalEvaluatorTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<TitleTarget>,
}

fn read_overrides(dir: &StateDir) -> BTreeMap<ModelTier, String> {
    state::get(dir, SCOPE_GLOBAL, TIERS)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read tier overrides");
            None
        })
        .unwrap_or_default()
}

fn read_roles(dir: &StateDir) -> PersistedRoles {
    state::get(dir, SCOPE_GLOBAL, ROLES)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read model roles");
            None
        })
        .unwrap_or_default()
}

fn update_persisted_roles(dir: &StateDir, update: impl FnOnce(&mut PersistedRoles)) {
    if let Err(error) = state::update(dir, SCOPE_GLOBAL, ROLES, update) {
        warn!(%error, "failed to persist model roles");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn make_map(overrides: &[(ModelTier, &str)], models: &[&str]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_overrides(overrides.iter().map(|(t, s)| (*t, s.to_string())).collect());
        if !models.is_empty() {
            reg.set_known_models(
                "ollama",
                models
                    .iter()
                    .map(|s| ModelInfo::id_only(s.to_string()))
                    .collect(),
            );
        }
        reg
    }

    const LEGACY_TIERS_FILE: &str = "model-tiers";
    const LEGACY_ROLES_FILE: &str = "model-roles";
    const IGNORED: &str = "a file from before consolidation must not be read";

    #[test]
    fn tier_and_role_files_from_before_consolidation_are_ignored() {
        let (_temp, dir) = state_dir();
        std::fs::write(dir.path().join(LEGACY_TIERS_FILE), "legacy/model=weak").unwrap();
        std::fs::write(
            dir.path().join(LEGACY_ROLES_FILE),
            "goal_evaluator=legacy/model",
        )
        .unwrap();

        assert!(read_overrides(&dir).is_empty(), "{IGNORED}");
        let roles = read_roles(&dir);
        assert!(roles.goal_evaluator.is_none(), "{IGNORED}");
        assert!(roles.compaction.is_none(), "{IGNORED}");
        assert!(roles.title.is_none(), "{IGNORED}");
    }

    #[test]
    fn tier_for_resolution_priority() {
        let mut reg = make_map(&[], &["pos0", "pos1", "pos2"]);
        reg.set("ollama/pos1".into(), ModelTier::Weak);

        let t = |spec, static_tier| reg.tier_for(spec, "ollama", static_tier);

        assert_eq!(t("ollama/pos1", Some(ModelTier::Strong)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", Some(ModelTier::Weak)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", None), ModelTier::Strong);
        assert_eq!(t("ollama/pos1", None), ModelTier::Weak);
        assert_eq!(t("ollama/pos2", None), ModelTier::Weak);
        assert_eq!(t("ollama/unknown", None), ModelTier::Medium);
    }

    #[test]
    fn curated_provider_ignores_discovered_tiers() {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            "synthetic",
            vec![ModelInfo {
                tier: Some(ModelTier::Strong),
                ..ModelInfo::id_only("syn:large:vision".into())
            }],
        );

        assert_ne!(
            reg.tier_for("synthetic/syn:large:vision", "synthetic", None),
            ModelTier::Strong
        );
        assert_ne!(
            reg.spec_for_tier("synthetic", ModelTier::Strong),
            Some("synthetic/syn:large:vision".into())
        );
    }

    fn make_tiered(models: &[(&str, ModelTier)]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            "copilot",
            models
                .iter()
                .map(|&(id, tier)| ModelInfo {
                    tier: Some(tier),
                    ..ModelInfo::id_only(id.into())
                })
                .collect(),
        );
        reg
    }

    #[test]
    fn discovered_category_tier_beats_position_and_static_fallback() {
        let reg = make_tiered(&[
            ("terra", ModelTier::Medium),
            ("luna", ModelTier::Weak),
            ("gpt-5.6-sol", ModelTier::Strong),
        ]);

        assert_eq!(
            reg.tier_for("copilot/gpt-5.6-sol", "copilot", Some(ModelTier::Medium)),
            ModelTier::Strong
        );
        assert_eq!(
            reg.tier_for("copilot/terra", "copilot", None),
            ModelTier::Medium
        );
        assert_eq!(
            reg.tier_for("copilot/luna", "copilot", None),
            ModelTier::Weak
        );
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some("copilot/gpt-5.6-sol".into())
        );
    }

    #[test_case(&[("gpt-5.4", ModelTier::Strong), ("claude-opus-4.7", ModelTier::Strong)], "copilot/claude-opus-4.7"; "curated default beats discovered tier")]
    #[test_case(&[("claude-opus-4.6", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/claude-opus-4.6"; "later curated prefix when first is unavailable")]
    #[test_case(&[("zeta", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/alpha"; "lowest id when no curated default is entitled")]
    fn spec_for_tier_prefers_entitled_curated_default(
        models: &[(&str, ModelTier)],
        expected: &str,
    ) {
        let reg = make_tiered(models);
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some(expected.into())
        );
    }

    #[test]
    fn spec_for_tier_ignores_discovery_list_order() {
        let models = [
            ("zeta", ModelTier::Strong),
            ("alpha", ModelTier::Strong),
            ("mid", ModelTier::Medium),
        ];
        let mut reversed = models;
        reversed.reverse();

        assert_eq!(
            make_tiered(&models).spec_for_tier("copilot", ModelTier::Strong),
            make_tiered(&reversed).spec_for_tier("copilot", ModelTier::Strong)
        );
    }

    #[test]
    fn tier_for_prefers_strongest_preset_assignment() {
        let mut reg = make_map(&[], &[]);
        reg.set("ollama/multi".into(), ModelTier::Medium);
        reg.set("ollama/multi".into(), ModelTier::Strong);

        let t = |spec| reg.tier_for(spec, "ollama", None);

        assert_eq!(t("ollama/multi"), ModelTier::Strong);
    }

    #[test]
    fn spec_for_tier_resolution() {
        let reg = make_map(
            &[(ModelTier::Strong, "ollama/custom")],
            &["big", "mid", "small"],
        );
        let s = |t| reg.spec_for_tier("ollama", t);

        assert_eq!(s(ModelTier::Strong), Some("ollama/custom".into()));
        assert_eq!(s(ModelTier::Medium), Some("ollama/mid".into()));
        assert_eq!(s(ModelTier::Weak), Some("ollama/small".into()));

        let scoped = make_map(&[(ModelTier::Strong, "openai/gpt-foo")], &[]);
        assert_eq!(scoped.spec_for_tier("ollama", ModelTier::Strong), None);

        let conflict = make_map(&[(ModelTier::Weak, "ollama/big")], &["big", "mid", "small"]);
        assert_eq!(conflict.spec_for_tier("ollama", ModelTier::Strong), None);
    }

    #[test]
    fn preset_overrides_are_global_exact_models() {
        let reg = make_map(
            &[
                (ModelTier::Weak, "zai/glm-5"),
                (ModelTier::Strong, "openai/gpt-foo"),
            ],
            &["big", "mid", "small"],
        );
        assert_eq!(reg.overrides[&ModelTier::Strong], "openai/gpt-foo");
        assert_eq!(reg.overrides[&ModelTier::Weak], "zai/glm-5");
        assert_eq!(
            reg.spec_for_tier("ollama", ModelTier::Medium),
            Some("ollama/mid".into())
        );
    }

    #[test]
    fn discovered_looks_up_by_id() {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            "llama-cpp",
            vec![
                ModelInfo::id_only("model-a".into()),
                ModelInfo {
                    context_window: Some(128_000),
                    ..ModelInfo::id_only("model-b".into())
                },
            ],
        );
        let info = reg.discovered("llama-cpp", "model-b").unwrap();
        assert_eq!(info.context_window, Some(128_000));
        assert!(reg.discovered("llama-cpp", "model-x").is_none());
        assert!(reg.discovered("ollama", "model-a").is_none());
    }

    #[test]
    fn tier_state_updates_merge_and_preserve_multi_tier_assignments() {
        let (_temp, dir) = state_dir();
        assert!(read_overrides(&dir).is_empty());

        update_persisted_overrides(&dir, |overrides| {
            overrides.insert(ModelTier::Strong, "ollama/qwen3".into());
        });
        update_persisted_overrides(&dir, |overrides| {
            overrides.insert(ModelTier::Medium, "ollama/qwen3".into());
        });
        update_persisted_overrides(&dir, |overrides| {
            overrides.insert(ModelTier::Weak, "ollama/qwen3:8b".into());
        });

        let loaded = read_overrides(&dir);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Medium).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Weak).unwrap(), "ollama/qwen3:8b");
    }

    #[test]
    fn role_state_updates_do_not_clobber_other_roles() {
        let (_temp, dir) = state_dir();
        let goal_evaluator = GoalEvaluatorTarget::Model("openai/gpt-5.4-nano".into());
        let compaction = CompactionTarget::Model("openai/gpt-5.4-mini".into());
        let title = TitleTarget::Model("openai/gpt-5.4-nano".into());

        update_persisted_roles(&dir, |roles| {
            roles.goal_evaluator = Some(goal_evaluator.clone());
        });
        update_persisted_roles(&dir, |roles| {
            roles.compaction = Some(compaction.clone());
        });
        update_persisted_roles(&dir, |roles| {
            roles.title = Some(title.clone());
        });

        let roles = read_roles(&dir);
        assert_eq!(roles.goal_evaluator, Some(goal_evaluator));
        assert_eq!(roles.compaction, Some(compaction.clone()));
        assert_eq!(roles.title, Some(title.clone()));

        update_persisted_roles(&dir, |roles| roles.goal_evaluator = None);
        let roles = read_roles(&dir);
        assert!(roles.goal_evaluator.is_none());
        assert_eq!(roles.compaction, Some(compaction));
        assert_eq!(roles.title, Some(title));
    }

    #[test]
    fn invalid_role_state_is_ignored() {
        let (_temp, dir) = state_dir();
        state::set(&dir, SCOPE_GLOBAL, ROLES, &"wrong type").unwrap();

        let roles = read_roles(&dir);
        assert!(roles.goal_evaluator.is_none());
        assert!(roles.compaction.is_none());
        assert!(roles.title.is_none());
    }

    #[test]
    fn unset_removes_matching_override() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Strong);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
        assert!(reg.overrides.is_empty());
    }

    #[test]
    fn unset_ignores_mismatched_spec() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/b", ModelTier::Strong);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn unset_ignores_mismatched_tier() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Weak);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn has_override_returns_false_for_no_override() {
        let reg = make_map(&[], &[]);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
    }
}
