//! The selected model and the short list of recent ones. Both are global state
//! rows.

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::sessions::StoredMode;
use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir};

/// The model last chosen in each mode, so planning and building can run on
/// different ones. Keyed by mode because the two are chosen for different work:
/// carrying a build pick into plan mode means planning on whatever happened to
/// write code last. Global rather than per project, matching the thinking level.
const SELECTED_BY_MODE: StateKey = StateKey {
    name: "model.selected_by_mode",
    class: StateClass::Persistent,
};

/// The single model chosen before the setting became per mode. Read as the
/// fallback for a mode with no choice of its own, so upgrading keeps the model
/// already in use instead of silently resetting to a default.
const SELECTED: StateKey = StateKey {
    name: "model.selected",
    class: StateClass::Persistent,
};
const RECENT: StateKey = StateKey {
    name: "model.recent",
    class: StateClass::Persistent,
};
const MAX_RECENTS: usize = 4;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ModeModels {
    plan: Option<String>,
    build: Option<String>,
}

impl ModeModels {
    fn slot(&mut self, mode: StoredMode) -> &mut Option<String> {
        match mode {
            StoredMode::Plan => &mut self.plan,
            StoredMode::Build => &mut self.build,
        }
    }

    /// The entry for `mode`, then the other mode's. The cross-mode fallback is
    /// what keeps a pair that never diverged behaving as one choice.
    fn resolve(mut self, mode: StoredMode) -> Option<String> {
        let other = match mode {
            StoredMode::Plan => StoredMode::Build,
            StoredMode::Build => StoredMode::Plan,
        };
        self.slot(mode).take().or_else(|| self.slot(other).take())
    }
}

pub fn persist_model(dir: &StateDir, mode: StoredMode, spec: &str) {
    let stored = state::update(
        dir,
        SCOPE_GLOBAL,
        SELECTED_BY_MODE,
        |models: &mut ModeModels| {
            *models.slot(mode) = Some(spec.to_owned());
        },
    );
    if let Err(error) = stored {
        warn!(%error, %mode, "failed to persist selected model");
    }
}

/// For a provider that was just configured: its default model is the choice for
/// every mode, because nothing about the setup names one.
pub fn persist_model_for_every_mode(dir: &StateDir, spec: &str) {
    let stored = state::set(
        dir,
        SCOPE_GLOBAL,
        SELECTED_BY_MODE,
        &ModeModels {
            plan: Some(spec.to_owned()),
            build: Some(spec.to_owned()),
        },
    );
    if let Err(error) = stored {
        warn!(%error, "failed to persist selected model");
    }
}

/// The model last chosen for `mode`, falling back to the other mode's choice and
/// then to the pre-split setting.
pub fn read_model(dir: &StateDir, mode: StoredMode) -> Option<String> {
    state::get::<ModeModels>(dir, SCOPE_GLOBAL, SELECTED_BY_MODE)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read selected models");
            None
        })
        .and_then(|models| models.resolve(mode))
        .or_else(|| {
            state::get::<String>(dir, SCOPE_GLOBAL, SELECTED).unwrap_or_else(|error| {
                warn!(%error, "failed to read selected model");
                None
            })
        })
        .filter(|spec| !spec.is_empty())
}

pub fn push_recent(dir: &StateDir, spec: &str) -> Vec<String> {
    let mut recents = read_recents(dir);
    recents.retain(|s| s != spec);
    recents.insert(0, spec.to_owned());
    recents.truncate(MAX_RECENTS);
    if let Err(error) = state::set(dir, SCOPE_GLOBAL, RECENT, &recents) {
        warn!(%error, "failed to persist recent models");
    }
    recents
}

pub fn read_recents(dir: &StateDir) -> Vec<String> {
    state::get::<Vec<String>>(dir, SCOPE_GLOBAL, RECENT)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read recent models");
            None
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PLAN_MODEL: &str = "anthropic/claude-opus-5";
    const BUILD_MODEL: &str = "anthropic/claude-sonnet-5";
    const LEGACY_MODEL: &str = "openai/gpt-5.4-nano";
    const MODES_LEAKED: &str = "a model chosen in one mode must not reach the other";
    const CHOICE_LOST: &str = "a mode without a choice must inherit one rather than answer nothing";

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    #[test_case(StoredMode::Plan ; "plan")]
    #[test_case(StoredMode::Build ; "build")]
    fn round_trip(mode: StoredMode) {
        let (_tmp, dir) = state_dir();

        assert!(read_model(&dir, mode).is_none());

        persist_model(&dir, mode, PLAN_MODEL);
        assert_eq!(read_model(&dir, mode).as_deref(), Some(PLAN_MODEL));

        persist_model(&dir, mode, LEGACY_MODEL);
        assert_eq!(read_model(&dir, mode).as_deref(), Some(LEGACY_MODEL));
    }

    #[test]
    fn a_model_chosen_in_one_mode_does_not_reach_the_other() {
        let (_tmp, dir) = state_dir();

        persist_model(&dir, StoredMode::Plan, PLAN_MODEL);
        persist_model(&dir, StoredMode::Build, BUILD_MODEL);

        assert_eq!(
            read_model(&dir, StoredMode::Plan).as_deref(),
            Some(PLAN_MODEL),
            "{MODES_LEAKED}"
        );
        assert_eq!(
            read_model(&dir, StoredMode::Build).as_deref(),
            Some(BUILD_MODEL),
            "{MODES_LEAKED}"
        );
    }

    #[test_case(StoredMode::Plan, StoredMode::Build ; "plan_inherits_build")]
    #[test_case(StoredMode::Build, StoredMode::Plan ; "build_inherits_plan")]
    fn a_mode_with_no_choice_inherits_the_other_modes_model(
        chosen: StoredMode,
        unchosen: StoredMode,
    ) {
        let (_tmp, dir) = state_dir();

        persist_model(&dir, chosen, PLAN_MODEL);

        assert_eq!(
            read_model(&dir, unchosen).as_deref(),
            Some(PLAN_MODEL),
            "{CHOICE_LOST}"
        );
    }

    /// The pre-split row is what an upgrading install carries, so both modes
    /// have to open on it rather than resetting to a config default.
    #[test_case(StoredMode::Plan ; "plan")]
    #[test_case(StoredMode::Build ; "build")]
    fn a_mode_with_no_choice_inherits_the_pre_split_selection(mode: StoredMode) {
        let (_tmp, dir) = state_dir();
        state::set(&dir, SCOPE_GLOBAL, SELECTED, &LEGACY_MODEL).unwrap();

        assert_eq!(
            read_model(&dir, mode).as_deref(),
            Some(LEGACY_MODEL),
            "{CHOICE_LOST}"
        );
    }

    /// A choice of its own outranks the pre-split row, or the upgrade would pin
    /// every mode to whatever was selected before it.
    #[test]
    fn a_chosen_model_outranks_the_pre_split_selection() {
        let (_tmp, dir) = state_dir();
        state::set(&dir, SCOPE_GLOBAL, SELECTED, &LEGACY_MODEL).unwrap();

        persist_model(&dir, StoredMode::Plan, PLAN_MODEL);

        assert_eq!(
            read_model(&dir, StoredMode::Plan).as_deref(),
            Some(PLAN_MODEL)
        );
    }

    #[test]
    fn persisting_for_every_mode_answers_both() {
        let (_tmp, dir) = state_dir();
        persist_model(&dir, StoredMode::Plan, PLAN_MODEL);

        persist_model_for_every_mode(&dir, BUILD_MODEL);

        assert_eq!(
            read_model(&dir, StoredMode::Plan).as_deref(),
            Some(BUILD_MODEL),
            "{MODES_LEAKED}"
        );
        assert_eq!(
            read_model(&dir, StoredMode::Build).as_deref(),
            Some(BUILD_MODEL),
            "{MODES_LEAKED}"
        );
    }

    #[test]
    fn push_recent_dedupes_and_orders_most_recent_first() {
        let (_tmp, dir) = state_dir();

        push_recent(&dir, PLAN_MODEL);
        push_recent(&dir, LEGACY_MODEL);
        assert_eq!(read_recents(&dir), [LEGACY_MODEL, PLAN_MODEL]);

        push_recent(&dir, PLAN_MODEL);
        assert_eq!(read_recents(&dir), [PLAN_MODEL, LEGACY_MODEL]);
    }

    #[test]
    fn push_recent_caps_at_max() {
        let (_tmp, dir) = state_dir();

        for i in 0..(MAX_RECENTS + 3) {
            push_recent(&dir, &format!("p/model-{i}"));
        }
        let recents = read_recents(&dir);
        assert_eq!(recents.len(), MAX_RECENTS);
        assert_eq!(recents[0], format!("p/model-{}", MAX_RECENTS + 2));
    }

    #[test]
    fn push_recent_returns_final_list() {
        let (_tmp, dir) = state_dir();

        let recents = push_recent(&dir, "a/b");
        assert_eq!(recents, vec!["a/b".to_string()]);
        assert_eq!(recents, read_recents(&dir));
    }

    #[test]
    fn read_recents_handles_missing_state() {
        let (_tmp, dir) = state_dir();

        assert!(read_recents(&dir).is_empty());
    }
}
