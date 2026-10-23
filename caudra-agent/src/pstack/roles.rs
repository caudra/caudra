//! The pstack model roles the user stores in `/settings`, and the table a session that loads
//! pstack reads them through: every kind resolved against the favorites its policy allows, then
//! the signed-in providers' flagships.

use caudra_config::ModelPolicy;
use caudra_providers::model::{Model, ModelMarker, ModelPurpose};
use caudra_providers::provider::signed_in_defaults;
pub use caudra_pstack::{
    AutoReason, DEFAULT_EFFORT, Kind, ResolvedSeat, RoleRow, RoleTable, Seat, Source, StoredRoles,
};
use caudra_pstack::{Candidate, ModelFacts};
use caudra_storage::state::{self, SCOPE_GLOBAL, StateKey};
use caudra_storage::{StateClass, StateDir, StorageError};
use tracing::warn;

use crate::agent::favorites;
use crate::agent::subagent::resolve_task_model_offline;

const ROLES_KEY: StateKey = StateKey {
    name: "pstack.roles",
    class: StateClass::Persistent,
};

/// What the user stored, or every kind on auto when nothing readable is.
pub fn load() -> StoredRoles {
    match StateDir::resolve() {
        Ok(dir) => load_from(&dir),
        Err(error) => {
            warn!(%error, key = ROLES_KEY.name, "no state directory, so every pstack role is auto");
            StoredRoles::default()
        }
    }
}

pub fn store(roles: &StoredRoles) -> Result<(), StorageError> {
    store_in(&StateDir::resolve()?, roles)
}

/// Read fresh on every call, so a change in `/settings` reaches the next page served.
pub fn table(policy: &ModelPolicy, task_model_choice: bool) -> RoleTable {
    resolve(&load(), policy, task_model_choice)
}

/// The thinking levels `spec` takes, low to high; empty when it has no control or is unknown.
pub fn effort_levels(spec: &str) -> Vec<String> {
    Model::from_spec(spec)
        .map(|model| favorites::effort_levels(&model))
        .unwrap_or_default()
}

fn load_from(dir: &StateDir) -> StoredRoles {
    match state::get::<StoredRoles>(dir, SCOPE_GLOBAL, ROLES_KEY) {
        Ok(stored) => stored.unwrap_or_default(),
        Err(error) => {
            warn!(%error, key = ROLES_KEY.name, "unreadable pstack roles, so every role is auto");
            StoredRoles::default()
        }
    }
}

fn store_in(dir: &StateDir, roles: &StoredRoles) -> Result<(), StorageError> {
    state::set(dir, SCOPE_GLOBAL, ROLES_KEY, roles)
}

fn resolve(stored: &StoredRoles, policy: &ModelPolicy, task_model_choice: bool) -> RoleTable {
    let known = |name: &str| candidate(&resolve_task_model_offline(policy, &[], name).ok()?);
    RoleTable::resolve(
        stored,
        &ModelFacts {
            favorites: allowed_favorites(policy),
            flagships: signed_in_defaults(ModelPurpose::Best, policy)
                .filter_map(|model| candidate(&model.spec()))
                .collect(),
            task_model_choice,
            known: &known,
        },
    )
}

fn allowed_favorites(policy: &ModelPolicy) -> Vec<Candidate> {
    favorites::allowed(policy)
        .iter()
        .filter_map(|spec| candidate(spec))
        .collect()
}

fn candidate(spec: &str) -> Option<Candidate> {
    let model = Model::from_spec(spec).ok()?;
    Some(Candidate {
        spec: spec.to_owned(),
        best_lane: Model::marker_of(&model.provider, &model.id) == Some(ModelMarker::Best),
        provider: model.provider.to_string(),
        levels: favorites::effort_levels(&model),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use caudra_providers::model_registry;
    use caudra_pstack::{AutoReason, Kind, ResolvedSeat, Seat, Source};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const OPUS: &str = "anthropic/claude-opus-5-5";
    const OPUS_SHORT: &str = "claude-opus-5-5";
    const HAIKU: &str = "anthropic/claude-haiku-5-5";
    const XHIGH: &str = "xhigh";
    const MAX: &str = "max";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn starred(dir: &StateDir) {
        for spec in [OPUS, HAIKU] {
            model_registry::toggle_favorite_and_persist(spec, dir).unwrap();
        }
    }

    fn only_haiku() -> ModelPolicy {
        ModelPolicy::new(&[HAIKU.to_owned()], &[]).unwrap()
    }

    #[test]
    fn stored_roles_survive_the_state_store() {
        let (_temp, dir) = state_dir();
        assert_eq!(load_from(&dir), StoredRoles::default());
        let roles = StoredRoles {
            effort: Some(MAX.to_owned()),
            kinds: BTreeMap::from([(Kind::Panel, vec![Seat::Inherit])]),
            ..StoredRoles::default()
        };
        store_in(&dir, &roles).unwrap();
        assert_eq!(load_from(&dir), roles);
    }

    #[test]
    fn unreadable_stored_roles_load_as_auto() {
        let (_temp, dir) = state_dir();
        state::set(&dir, SCOPE_GLOBAL, ROLES_KEY, &json!({ "kinds": 5 })).unwrap();
        assert_eq!(load_from(&dir), StoredRoles::default());
    }

    #[test]
    fn the_facts_hold_only_the_favorites_the_policy_allows() {
        let (_temp, dir) = state_dir();
        starred(&dir);
        let all: Vec<(String, bool)> = allowed_favorites(&ModelPolicy::default())
            .into_iter()
            .map(|favorite| (favorite.spec, favorite.best_lane))
            .collect();
        assert_eq!(all, [(HAIKU.to_owned(), false), (OPUS.to_owned(), true)]);
        let kept: Vec<String> = allowed_favorites(&only_haiku())
            .into_iter()
            .map(|favorite| favorite.spec)
            .collect();
        assert_eq!(kept, [HAIKU]);
    }

    #[test_case(ModelPolicy::default(), Some(OPUS), AutoReason::BestFavorite ; "an_allowed_best_favorite")]
    #[test_case(only_haiku(), None, AutoReason::NoModel ; "an_excluded_best_favorite")]
    fn the_hardest_kind_follows_the_policy(
        policy: ModelPolicy,
        model: Option<&str>,
        reason: AutoReason,
    ) {
        let (_temp, dir) = state_dir();
        starred(&dir);
        let table = resolve(&StoredRoles::default(), &policy, true);
        let row = table.row(Kind::Hardest).unwrap();
        assert_eq!(row.source, Source::Auto(reason));
        assert_eq!(row.seats[0].model.as_deref(), model);
    }

    #[test]
    fn a_stored_short_name_resolves_through_the_favorites() {
        let (_temp, dir) = state_dir();
        starred(&dir);
        let stored = StoredRoles {
            kinds: BTreeMap::from([(
                Kind::Code,
                vec![Seat::Model {
                    model: OPUS_SHORT.to_owned(),
                    thinking: None,
                }],
            )]),
            ..StoredRoles::default()
        };
        let table = resolve(&stored, &ModelPolicy::default(), true);
        let row = table.row(Kind::Code).unwrap();
        assert_eq!(row.source, Source::Set);
        assert_eq!(
            row.seats,
            [ResolvedSeat {
                model: Some(OPUS.to_owned()),
                thinking: Some(XHIGH.to_owned()),
            }]
        );
    }
}
