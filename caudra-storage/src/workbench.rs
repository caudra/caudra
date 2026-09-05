//! Where the workbench left off in a given checkout: its open tabs, which
//! sidebar was showing, and how wide it was. A project-scoped state row, so
//! two clones of the same repository keep their own layouts.
//!
//! The shape belongs to `caudra-workbench`, which is a UI crate this one must
//! not depend on, so the value stays generic and only the key and the scope
//! live here.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::state::{self, StateKey, project_scope};
use crate::{StateClass, StateDir};

const WORKBENCH: StateKey = StateKey {
    name: "ui.workbench",
    class: StateClass::Persistent,
};

pub fn persist<T: Serialize>(dir: &StateDir, cwd: &Path, layout: &T) {
    if let Err(error) = state::set(dir, &project_scope(cwd), WORKBENCH, layout) {
        warn!(%error, "failed to persist workbench layout");
    }
}

/// `None` when nothing was stored, or when what was stored no longer parses,
/// which leaves the caller's own default standing rather than failing to open.
pub fn read<T: DeserializeOwned>(dir: &StateDir, cwd: &Path) -> Option<T> {
    state::get(dir, &project_scope(cwd), WORKBENCH).unwrap_or_else(|error| {
        warn!(%error, "failed to read workbench layout");
        None
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde::Deserialize;
    use tempfile::TempDir;

    use super::{StateDir, WORKBENCH, persist, read, state};
    use crate::state::project_scope;

    const UNSET: &str = "an unwritten layout must leave the caller's default alone";
    const ROUND_TRIP: &str = "a stored layout must read back as written";
    const SCOPED: &str = "a layout belongs to the checkout it was written from";
    const UNREADABLE: &str = "a layout this build cannot parse is not a layout";

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
    struct Layout {
        tabs: Vec<String>,
        width: u16,
    }

    fn fixture() -> (TempDir, StateDir, std::path::PathBuf) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        (temp, dir, project)
    }

    fn layout() -> Layout {
        Layout {
            tabs: vec!["a.rs".to_owned()],
            width: 42,
        }
    }

    #[test]
    fn a_layout_round_trips_for_the_project_it_was_written_from() {
        let (_temp, dir, project) = fixture();
        assert_eq!(read::<Layout>(&dir, &project), None, "{UNSET}");

        persist(&dir, &project, &layout());
        assert_eq!(read(&dir, &project), Some(layout()), "{ROUND_TRIP}");
    }

    #[test]
    fn another_checkout_keeps_its_own_layout() {
        let (temp, dir, project) = fixture();
        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();

        persist(&dir, &project, &layout());
        assert_eq!(read::<Layout>(&dir, &other), None, "{SCOPED}");
    }

    #[test]
    fn a_layout_this_build_cannot_parse_is_ignored() {
        let (_temp, dir, project) = fixture();
        state::set(&dir, &project_scope(&project), WORKBENCH, &"not a layout").unwrap();

        assert_eq!(read::<Layout>(&dir, &project), None, "{UNREADABLE}");
    }
}
