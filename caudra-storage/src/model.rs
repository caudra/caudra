//! The last selected model and the short list of recent ones. Both are global
//! state rows.

use tracing::warn;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir};

const SELECTED: StateKey = StateKey {
    name: "model.selected",
    class: StateClass::Persistent,
};
const RECENT: StateKey = StateKey {
    name: "model.recent",
    class: StateClass::Persistent,
};
const MAX_RECENTS: usize = 4;

pub fn persist_model(dir: &StateDir, spec: &str) {
    if let Err(error) = state::set(dir, SCOPE_GLOBAL, SELECTED, &spec) {
        warn!(%error, "failed to persist selected model");
    }
}

pub fn read_model(dir: &StateDir) -> Option<String> {
    state::get::<String>(dir, SCOPE_GLOBAL, SELECTED)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read selected model");
            None
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

    use super::*;

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    #[test]
    fn round_trip() {
        let (_tmp, dir) = state_dir();

        assert!(read_model(&dir).is_none());

        persist_model(&dir, "anthropic/claude-sonnet-4");
        assert_eq!(
            read_model(&dir).as_deref(),
            Some("anthropic/claude-sonnet-4")
        );

        persist_model(&dir, "openai/gpt-5.4-nano");
        assert_eq!(read_model(&dir).as_deref(), Some("openai/gpt-5.4-nano"));
    }

    #[test]
    fn push_recent_dedupes_and_orders_most_recent_first() {
        let (_tmp, dir) = state_dir();

        push_recent(&dir, "anthropic/claude-sonnet-4");
        push_recent(&dir, "openai/gpt-5.4-nano");
        assert_eq!(
            read_recents(&dir),
            ["openai/gpt-5.4-nano", "anthropic/claude-sonnet-4"]
        );

        push_recent(&dir, "anthropic/claude-sonnet-4");
        assert_eq!(
            read_recents(&dir),
            ["anthropic/claude-sonnet-4", "openai/gpt-5.4-nano"]
        );
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
