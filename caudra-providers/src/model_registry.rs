//! Persisted bindings from a workload purpose to the model that serves it.
//!
//! A binding is an assignment the user made. Nothing here infers what a model
//! is; a purpose the user never bound stays absent, and the caller applies its
//! own default rule (see [`crate::model::Model::resolve`]).
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

use caudra_config::providers::ModelPurpose;
use caudra_storage::state::{self, SCOPE_GLOBAL, StateKey};
use caudra_storage::{StateClass, StateDir, StorageError};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::warn;

use crate::model::ModelInfo;
use crate::providers::local::OllamaModelInfo;

const PURPOSES: StateKey = StateKey {
    name: "model.purposes",
    class: StateClass::Persistent,
};

static REGISTRY: OnceLock<RwLock<ModelRegistry>> = OnceLock::new();

/// What a purpose points at. Absent from the table means "no binding", which is
/// distinct from either variant: the caller falls back to its own default rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Binding {
    /// A `provider/model-id` spec, which may name another provider.
    Exact(String),
    /// Whatever another purpose resolves to.
    Same(ModelPurpose),
}

#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    #[error(
        "model purpose '{purpose}' cannot bind to '{target}'; expected a target of chat, plan, fast, or best"
    )]
    InvalidTarget {
        purpose: ModelPurpose,
        target: ModelPurpose,
    },
    #[error("binding model purpose '{purpose}' to '{target}' would create a cycle")]
    Cycle {
        purpose: ModelPurpose,
        target: ModelPurpose,
    },
    #[error("model purpose storage: {0}")]
    Storage(#[from] StorageError),
}

#[derive(Debug, Default, Serialize)]
#[serde(transparent)]
struct StoredBindings(BTreeMap<ModelPurpose, Binding>);

impl<'de> Deserialize<'de> for StoredBindings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let stored = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        let mut bindings = BTreeMap::new();
        for (stored_purpose, value) in stored {
            let Ok(purpose) = stored_purpose.parse::<ModelPurpose>() else {
                warn!(
                    purpose = stored_purpose,
                    "skipping stale model purpose binding"
                );
                continue;
            };
            match serde_json::from_value(value) {
                Ok(binding) => {
                    bindings.insert(purpose, binding);
                }
                Err(error) => {
                    warn!(purpose = stored_purpose, %error, "skipping stale model purpose binding");
                }
            }
        }
        Ok(Self(bindings))
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(spec) => f.write_str(spec),
            Self::Same(purpose) => write!(f, "same as {purpose}"),
        }
    }
}

fn read() -> RwLockReadGuard<'static, ModelRegistry> {
    registry().read().unwrap()
}

fn write() -> RwLockWriteGuard<'static, ModelRegistry> {
    registry().write().unwrap()
}

fn registry() -> &'static RwLock<ModelRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ModelRegistry::default()))
}

pub fn binding(purpose: ModelPurpose) -> Option<Binding> {
    read().bindings.get(&purpose).cloned()
}

/// Purposes bound directly to `spec`, so a picker can show which slots claim a
/// row. `Same` bindings are not followed: they claim a purpose, not a model.
pub fn purposes_bound_to(spec: &str) -> Vec<ModelPurpose> {
    let mut purposes: Vec<_> = read()
        .bindings
        .iter()
        .filter(|(_, b)| matches!(b, Binding::Exact(s) if s == spec))
        .map(|(&purpose, _)| purpose)
        .collect();
    purposes.sort_unstable();
    purposes
}

/// Whether pointing `purpose` at `target` would enter a chain of `Same`
/// bindings that loops instead of reaching a default or exact model.
pub fn binding_would_cycle(purpose: ModelPurpose, target: ModelPurpose) -> bool {
    read().binding_would_cycle(purpose, target)
}

pub fn discovered(provider: &str, model_id: &str) -> Option<ModelInfo> {
    read().discovered(provider, model_id).cloned()
}

/// Typed `provider_info` for a discovered model, keyed by its routing slug.
pub fn provider_info<T: Send + Sync + 'static>(provider: &str, model_id: &str) -> Option<Arc<T>> {
    let info = read()
        .discovered(provider, model_id)?
        .provider_info
        .clone()?;
    Arc::downcast(info).ok()
}

/// Cheapest model the provider reported a real price for, as a bare model id.
///
/// Unknown and all-zero prices are skipped rather than read as free: local and
/// custom endpoints report zero for "no billing here", which would otherwise let
/// them win every comparison. Ties break on id so the answer is stable across
/// discovery reordering.
pub fn cheapest_known(provider: &str) -> Option<String> {
    read().cheapest_known(provider)
}

/// Fewest parameters the provider reported, as a bare model id.
///
/// A local runtime prices everything at zero, so [`cheapest_known`] has nothing
/// to sort on there. Size is the cheapness signal that remains, and only for the
/// Fast slot: a 3B model is reliably cheap to run, while nothing about parameter
/// count establishes that a bigger one is more capable.
pub fn smallest_known(provider: &str) -> Option<String> {
    read().smallest_known(provider)
}

pub fn set_known_models(provider: &str, models: Vec<ModelInfo>) {
    write().set_known_models(provider, models);
}

pub fn load_from_storage(dir: &StateDir) -> Result<(), BindingError> {
    write().load_from_storage(dir)
}

pub fn set_binding_and_persist(
    purpose: ModelPurpose,
    binding: Binding,
    dir: &StateDir,
) -> Result<(), BindingError> {
    write().update_and_persist(dir, move |bindings| {
        bindings.insert(purpose, binding);
    })
}

pub fn clear_binding_and_persist(
    purpose: ModelPurpose,
    dir: &StateDir,
) -> Result<(), BindingError> {
    write().update_and_persist(dir, move |bindings| {
        bindings.remove(&purpose);
    })
}

fn read_bindings(dir: &StateDir) -> Result<BTreeMap<ModelPurpose, Binding>, BindingError> {
    let bindings = state::get::<StoredBindings>(dir, SCOPE_GLOBAL, PURPOSES)?
        .map(|stored| stored.0)
        .unwrap_or_default();
    validate_bindings(&bindings)?;
    Ok(bindings)
}

fn validate_bindings(bindings: &BTreeMap<ModelPurpose, Binding>) -> Result<(), BindingError> {
    for (&purpose, binding) in bindings {
        let Binding::Same(target) = binding else {
            continue;
        };
        if !ModelPurpose::TARGETS.contains(target) {
            return Err(BindingError::InvalidTarget {
                purpose,
                target: *target,
            });
        }
        if would_cycle_in(bindings, purpose, *target) {
            return Err(BindingError::Cycle {
                purpose,
                target: *target,
            });
        }
    }
    Ok(())
}

fn would_cycle_in(
    bindings: &BTreeMap<ModelPurpose, Binding>,
    purpose: ModelPurpose,
    target: ModelPurpose,
) -> bool {
    let mut seen = vec![purpose];
    let mut current = target;
    loop {
        if seen.contains(&current) {
            return true;
        }
        seen.push(current);
        match bindings.get(&current) {
            Some(Binding::Same(next)) => current = *next,
            Some(Binding::Exact(_)) | None => return false,
        }
    }
}

fn persist_update(
    dir: &StateDir,
    update: impl FnOnce(&mut BTreeMap<ModelPurpose, Binding>),
) -> Result<BTreeMap<ModelPurpose, Binding>, BindingError> {
    state::try_update(
        dir,
        SCOPE_GLOBAL,
        PURPOSES,
        |stored: &mut StoredBindings| {
            update(&mut stored.0);
            validate_bindings(&stored.0)?;
            Ok(stored.0.clone())
        },
    )?
}

#[derive(Debug, Default)]
struct ModelRegistry {
    /// Keyed by purpose (not spec) so binding a model automatically evicts the
    /// previous holder of that slot. Persisted to disk.
    bindings: BTreeMap<ModelPurpose, Binding>,
    /// Ordered model info per provider, populated from `list_models()`.
    /// Not persisted, rebuilt every session. Discovered metadata lookup only.
    known_models: HashMap<String, Vec<ModelInfo>>,
}

impl ModelRegistry {
    fn load_from_storage(&mut self, dir: &StateDir) -> Result<(), BindingError> {
        let bindings = read_bindings(dir)?;
        self.bindings = bindings;
        Ok(())
    }

    fn update_and_persist(
        &mut self,
        dir: &StateDir,
        update: impl FnOnce(&mut BTreeMap<ModelPurpose, Binding>),
    ) -> Result<(), BindingError> {
        let bindings = persist_update(dir, update)?;
        self.bindings = bindings;
        Ok(())
    }

    fn binding_would_cycle(&self, purpose: ModelPurpose, target: ModelPurpose) -> bool {
        would_cycle_in(&self.bindings, purpose, target)
    }

    fn set_known_models(&mut self, provider: &str, models: Vec<ModelInfo>) {
        self.known_models.insert(provider.to_string(), models);
    }

    fn discovered(&self, provider: &str, model_id: &str) -> Option<&ModelInfo> {
        self.known_models
            .get(provider)?
            .iter()
            .find(|m| m.id == model_id)
    }

    fn cheapest_known(&self, provider: &str) -> Option<String> {
        self.known_models
            .get(provider)?
            .iter()
            .filter_map(|model| {
                let pricing = model.pricing.as_ref().filter(|p| !p.is_zero())?;
                Some((pricing.input + pricing.output, model.id.as_str()))
            })
            .min_by(|(a_cost, a_id), (b_cost, b_id)| {
                a_cost.total_cmp(b_cost).then_with(|| a_id.cmp(b_id))
            })
            .map(|(_, id)| id.to_string())
    }

    fn smallest_known(&self, provider: &str) -> Option<String> {
        self.known_models
            .get(provider)?
            .iter()
            .filter_map(|model| {
                let info = model
                    .provider_info
                    .as_ref()?
                    .downcast_ref::<OllamaModelInfo>()?;
                Some((info.parameter_count?, model.id.as_str()))
            })
            .min()
            .map(|(_, id)| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelPricing;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    const CHEAP: &str = "cheap";
    const DEAR: &str = "dear";
    const FREE: &str = "free";
    const PROVIDER: &str = "ollama";
    const UNKNOWN_PRICE_SKIPPED: &str =
        "a model with no reported price must not win a price comparison";
    const NO_MIGRATION: &str = "loading must not migrate persisted state";
    const MEMORY_CHANGED: &str = "a rejected durable update must not change memory";
    const STORAGE_CHANGED: &str = "a rejected durable update must not change storage";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn priced(id: &str, input: f64, output: f64) -> ModelInfo {
        ModelInfo {
            pricing: Some(ModelPricing {
                input,
                output,
                ..ModelPricing::ZERO
            }),
            ..ModelInfo::id_only(id.to_string())
        }
    }

    fn registry_with(models: Vec<ModelInfo>) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(PROVIDER, models);
        reg
    }

    #[test]
    fn cheapest_known_sums_input_and_output() {
        let reg = registry_with(vec![
            priced(DEAR, 10.0, 30.0),
            priced(CHEAP, 1.0, 2.0),
            priced("mid", 3.0, 5.0),
        ]);

        assert_eq!(reg.cheapest_known(PROVIDER).as_deref(), Some(CHEAP));
    }

    #[test]
    fn cheapest_known_skips_unpriced_and_zero_priced_models() {
        let reg = registry_with(vec![
            ModelInfo::id_only(FREE.to_string()),
            ModelInfo {
                pricing: Some(ModelPricing::ZERO),
                ..ModelInfo::id_only("zeroed".to_string())
            },
            priced(DEAR, 10.0, 30.0),
        ]);

        assert_eq!(
            reg.cheapest_known(PROVIDER).as_deref(),
            Some(DEAR),
            "{UNKNOWN_PRICE_SKIPPED}"
        );
    }

    /// Equal prices must not resolve by discovery order, or the answer changes
    /// when the provider reorders its list.
    #[test]
    fn cheapest_known_breaks_ties_on_id() {
        let reg = registry_with(vec![priced(DEAR, 1.0, 1.0), priced(CHEAP, 1.0, 1.0)]);

        assert_eq!(reg.cheapest_known(PROVIDER).as_deref(), Some(CHEAP));
    }

    #[test]
    fn cheapest_known_is_absent_when_nothing_is_priced() {
        let reg = registry_with(vec![ModelInfo::id_only(FREE.to_string())]);

        assert_eq!(reg.cheapest_known(PROVIDER), None);
    }

    #[test]
    fn unknown_provider_has_no_cheapest() {
        assert_eq!(ModelRegistry::default().cheapest_known(PROVIDER), None);
    }

    const SMALL: &str = "small";
    const LARGE: &str = "large";
    const SIZE_OVER_ORDER: &str =
        "the Fast slot on a local runtime must sort on size, not discovery order";

    fn sized(id: &str, parameter_count: u64) -> ModelInfo {
        ModelInfo {
            pricing: Some(ModelPricing::ZERO),
            provider_info: Some(Arc::new(OllamaModelInfo {
                parameter_count: Some(parameter_count),
            })),
            ..ModelInfo::id_only(id.to_string())
        }
    }

    #[test]
    fn smallest_known_picks_the_fewest_parameters() {
        let reg = registry_with(vec![
            sized(LARGE, 70_000_000_000),
            sized(SMALL, 3_000_000_000),
        ]);

        assert_eq!(
            reg.smallest_known(PROVIDER).as_deref(),
            Some(SMALL),
            "{SIZE_OVER_ORDER}"
        );
    }

    /// A local runtime reports zero for everything, so the price rule finds
    /// nothing and size is the only signal left.
    #[test]
    fn zero_priced_models_have_no_cheapest_but_do_have_a_smallest() {
        let reg = registry_with(vec![
            sized(LARGE, 70_000_000_000),
            sized(SMALL, 3_000_000_000),
        ]);

        assert_eq!(reg.cheapest_known(PROVIDER), None);
        assert_eq!(reg.smallest_known(PROVIDER).as_deref(), Some(SMALL));
    }

    #[test]
    fn models_without_a_reported_size_have_no_smallest() {
        let reg = registry_with(vec![ModelInfo::id_only(SMALL.to_string())]);

        assert_eq!(reg.smallest_known(PROVIDER), None);
    }

    #[test_case(Binding::Exact("openai/gpt-x".into()) ; "exact")]
    #[test_case(Binding::Same(ModelPurpose::Fast) ; "same")]
    fn bindings_survive_a_persistence_roundtrip(binding: Binding) {
        let (_temp, dir) = state_dir();
        let mut stored = BTreeMap::new();
        stored.insert(ModelPurpose::Title, binding.clone());
        state::update(&dir, SCOPE_GLOBAL, PURPOSES, |b| *b = stored).unwrap();

        assert_eq!(
            read_bindings(&dir).unwrap().get(&ModelPurpose::Title),
            Some(&binding)
        );
    }

    #[test]
    fn stale_balanced_bindings_are_skipped_without_rewriting_storage() {
        let (_temp, dir) = state_dir();
        let stored = serde_json::json!({
            "balanced": {"kind": "exact", "value": "old/model"},
            "fast": {"kind": "exact", "value": "small/model"},
            "title": {"kind": "same", "value": "balanced"}
        });
        state::set(&dir, SCOPE_GLOBAL, PURPOSES, &stored).unwrap();

        let bindings = read_bindings(&dir).unwrap();
        assert_eq!(
            bindings.get(&ModelPurpose::Fast),
            Some(&Binding::Exact("small/model".into()))
        );
        assert_eq!(bindings.get(&ModelPurpose::Title), None);
        let unchanged: serde_json::Value =
            state::get(&dir, SCOPE_GLOBAL, PURPOSES).unwrap().unwrap();
        assert_eq!(unchanged, stored, "{NO_MIGRATION}");
    }

    #[test]
    fn absent_binding_reads_as_none_rather_than_a_default() {
        let (_temp, dir) = state_dir();

        assert_eq!(read_bindings(&dir).unwrap().get(&ModelPurpose::Goal), None);
    }

    #[test]
    fn only_exact_bindings_claim_a_spec() {
        let mut reg = ModelRegistry::default();
        reg.bindings
            .insert(ModelPurpose::Fast, Binding::Exact("p/m".into()));
        reg.bindings
            .insert(ModelPurpose::Title, Binding::Same(ModelPurpose::Fast));

        let claimed: Vec<_> = reg
            .bindings
            .iter()
            .filter(|(_, b)| matches!(b, Binding::Exact(s) if s == "p/m"))
            .map(|(&p, _)| p)
            .collect();
        assert_eq!(claimed, vec![ModelPurpose::Fast]);
    }

    #[test]
    fn cycle_check_follows_transitive_same_bindings() {
        let mut reg = ModelRegistry::default();
        reg.bindings
            .insert(ModelPurpose::Plan, Binding::Same(ModelPurpose::Fast));
        reg.bindings
            .insert(ModelPurpose::Fast, Binding::Same(ModelPurpose::Goal));

        assert!(reg.binding_would_cycle(ModelPurpose::Goal, ModelPurpose::Plan));
        assert!(reg.binding_would_cycle(ModelPurpose::Goal, ModelPurpose::Goal));
        assert!(!reg.binding_would_cycle(ModelPurpose::Goal, ModelPurpose::Chat));
    }

    #[test_case(ModelPurpose::Subagent ; "subagent")]
    #[test_case(ModelPurpose::Compact ; "compact")]
    #[test_case(ModelPurpose::Title ; "title")]
    #[test_case(ModelPurpose::Goal ; "goal")]
    fn same_bindings_reject_non_target_purposes(target: ModelPurpose) {
        let bindings = BTreeMap::from([(ModelPurpose::Chat, Binding::Same(target))]);

        assert!(matches!(
            validate_bindings(&bindings),
            Err(BindingError::InvalidTarget {
                purpose: ModelPurpose::Chat,
                target: rejected,
            }) if rejected == target
        ));
    }

    #[test]
    fn same_bindings_reject_self_and_transitive_cycles() {
        let self_cycle = BTreeMap::from([(ModelPurpose::Chat, Binding::Same(ModelPurpose::Chat))]);
        assert!(matches!(
            validate_bindings(&self_cycle),
            Err(BindingError::Cycle { .. })
        ));

        let transitive = BTreeMap::from([
            (ModelPurpose::Chat, Binding::Same(ModelPurpose::Plan)),
            (ModelPurpose::Plan, Binding::Same(ModelPurpose::Fast)),
            (ModelPurpose::Fast, Binding::Same(ModelPurpose::Chat)),
        ]);
        assert!(matches!(
            validate_bindings(&transitive),
            Err(BindingError::Cycle { .. })
        ));
    }

    #[test]
    fn invalid_persisted_bindings_leave_loaded_memory_unchanged() {
        let (_temp, dir) = state_dir();
        let invalid = BTreeMap::from([(ModelPurpose::Fast, Binding::Same(ModelPurpose::Title))]);
        state::set(&dir, SCOPE_GLOBAL, PURPOSES, &invalid).unwrap();
        let original = BTreeMap::from([(
            ModelPurpose::Chat,
            Binding::Exact("provider/original".into()),
        )]);
        let mut reg = ModelRegistry {
            bindings: original.clone(),
            ..Default::default()
        };

        assert!(matches!(
            reg.load_from_storage(&dir),
            Err(BindingError::InvalidTarget { .. })
        ));
        assert_eq!(reg.bindings, original, "{MEMORY_CHANGED}");
    }

    #[test]
    fn rejected_cycle_changes_neither_storage_nor_memory() {
        let (_temp, dir) = state_dir();
        let original = BTreeMap::from([(ModelPurpose::Chat, Binding::Same(ModelPurpose::Plan))]);
        state::set(&dir, SCOPE_GLOBAL, PURPOSES, &original).unwrap();
        let mut reg = ModelRegistry {
            bindings: original.clone(),
            ..Default::default()
        };

        assert!(matches!(
            reg.update_and_persist(&dir, |bindings| {
                bindings.insert(ModelPurpose::Plan, Binding::Same(ModelPurpose::Chat));
            }),
            Err(BindingError::Cycle { .. })
        ));
        assert_eq!(reg.bindings, original, "{MEMORY_CHANGED}");
        assert_eq!(read_bindings(&dir).unwrap(), original, "{STORAGE_CHANGED}");
    }

    #[test]
    fn storage_failure_is_returned_before_memory_changes() {
        let temp = TempDir::new().unwrap();
        let blocked = temp.path().join("not-a-directory");
        fs::write(&blocked, "blocked").unwrap();
        let dir = StateDir::from_path(blocked);
        let original = BTreeMap::from([(
            ModelPurpose::Chat,
            Binding::Exact("provider/original".into()),
        )]);
        let mut reg = ModelRegistry {
            bindings: original.clone(),
            ..Default::default()
        };

        assert!(matches!(
            reg.update_and_persist(&dir, |bindings| {
                bindings.insert(ModelPurpose::Fast, Binding::Exact("provider/new".into()));
            }),
            Err(BindingError::Storage(_))
        ));
        assert_eq!(reg.bindings, original, "{MEMORY_CHANGED}");
    }
}
