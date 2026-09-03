//! The theme picked from `/theme`, as a global state row.

use tracing::warn;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir};

const THEME: StateKey = StateKey {
    name: "ui.theme",
    class: StateClass::Persistent,
};
pub fn persist_theme_name(dir: &StateDir, name: &str) {
    if let Err(error) = state::set(dir, SCOPE_GLOBAL, THEME, &name) {
        warn!(%error, "failed to persist theme name");
    }
}

pub fn read_theme_name(dir: &StateDir) -> Option<String> {
    state::get::<String>(dir, SCOPE_GLOBAL, THEME)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read theme name");
            None
        })
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn theme_persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(read_theme_name(&dir).is_none());

        persist_theme_name(&dir, "gruvbox");
        assert_eq!(read_theme_name(&dir).as_deref(), Some("gruvbox"));
    }
}
