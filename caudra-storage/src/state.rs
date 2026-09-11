//! Small durable values beside the sessions: model choices, view and theme,
//! input history, the prompt stash, permission rules, trust digests, and the
//! open tabs of a workspace. Each is one JSON row in the `state` table of the
//! session database, so they share its lock, durability, and migration path.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::id::CaudraId;
use crate::sessions::SessionDatabase;
use crate::{StateClass, StateDir, StorageError};

pub const SCOPE_GLOBAL: &str = "global";
const PROJECT_SCOPE_PREFIX: &str = "project:";
const WORKSPACE_TABS_KEY: StateKey = StateKey {
    name: "workspace.tabs",
    class: StateClass::Persistent,
};

/// One stored value: its row name and which root holds it in an ephemeral run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateKey {
    pub name: &'static str,
    pub class: StateClass,
}

/// A repository handle for several state operations in a row. Opening one
/// costs a connection, so callers with one read or write use the free
/// functions instead.
pub struct StateStore {
    database: SessionDatabase,
}

impl StateStore {
    pub fn open(dir: &StateDir, class: StateClass) -> Result<Self, StorageError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(class))?,
        })
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        scope: &str,
        key: StateKey,
    ) -> Result<Option<T>, StorageError> {
        Ok(self.database.state_get(scope, key.name)?)
    }

    pub fn set<T: Serialize>(
        &self,
        scope: &str,
        key: StateKey,
        value: &T,
    ) -> Result<(), StorageError> {
        Ok(self.database.state_set(scope, key.name, value)?)
    }

    pub fn delete(&self, scope: &str, key: StateKey) -> Result<bool, StorageError> {
        Ok(self.database.state_delete(scope, key.name)?)
    }

    pub fn update<T, R>(
        &mut self,
        scope: &str,
        key: StateKey,
        update: impl FnOnce(&mut T) -> R,
    ) -> Result<R, StorageError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        Ok(self.database.state_update(scope, key.name, update)?)
    }

    pub fn try_update<T, R, E>(
        &mut self,
        scope: &str,
        key: StateKey,
        update: impl FnOnce(&mut T) -> Result<R, E>,
    ) -> Result<Result<R, E>, StorageError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        Ok(self.database.state_try_update(scope, key.name, update)?)
    }
}

pub fn get<T: DeserializeOwned>(
    dir: &StateDir,
    scope: &str,
    key: StateKey,
) -> Result<Option<T>, StorageError> {
    StateStore::open(dir, key.class)?.get(scope, key)
}

pub fn set<T: Serialize>(
    dir: &StateDir,
    scope: &str,
    key: StateKey,
    value: &T,
) -> Result<(), StorageError> {
    StateStore::open(dir, key.class)?.set(scope, key, value)
}

pub fn delete(dir: &StateDir, scope: &str, key: StateKey) -> Result<bool, StorageError> {
    StateStore::open(dir, key.class)?.delete(scope, key)
}

pub fn update<T, R>(
    dir: &StateDir,
    scope: &str,
    key: StateKey,
    update: impl FnOnce(&mut T) -> R,
) -> Result<R, StorageError>
where
    T: DeserializeOwned + Serialize + Default,
{
    StateStore::open(dir, key.class)?.update(scope, key, update)
}

pub fn try_update<T, R, E>(
    dir: &StateDir,
    scope: &str,
    key: StateKey,
    update: impl FnOnce(&mut T) -> Result<R, E>,
) -> Result<Result<R, E>, StorageError>
where
    T: DeserializeOwned + Serialize + Default,
{
    StateStore::open(dir, key.class)?.try_update(scope, key, update)
}

/// The scope for values that belong to one working directory.
pub fn project_scope(cwd: &Path) -> String {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    format!("{PROJECT_SCOPE_PREFIX}{}", canonical.display())
}

/// The sessions a workspace had open, so `--continue` restores the layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTabs {
    pub open: Vec<CaudraId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<CaudraId>,
}

pub fn read_workspace_tabs(
    dir: &StateDir,
    cwd: &Path,
) -> Result<Option<WorkspaceTabs>, StorageError> {
    get(dir, &project_scope(cwd), WORKSPACE_TABS_KEY)
}

/// A no-op during an ephemeral run: its session ids would be dangling
/// references in the persistent root.
pub fn write_workspace_tabs(
    dir: &StateDir,
    cwd: &Path,
    tabs: &WorkspaceTabs,
) -> Result<(), StorageError> {
    if dir.is_ephemeral() {
        return Ok(());
    }
    set(dir, &project_scope(cwd), WORKSPACE_TABS_KEY, tabs)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const KEY: StateKey = StateKey {
        name: "test.value",
        class: StateClass::Persistent,
    };
    const VOLATILE_KEY: StateKey = StateKey {
        name: "test.volatile",
        class: StateClass::Volatile,
    };
    const EPHEMERAL_SPLIT: &str = "volatile values must not reach the persistent root";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    #[test]
    fn set_get_and_update_round_trip() {
        let (_temp, dir) = state_dir();
        assert_eq!(get::<u32>(&dir, SCOPE_GLOBAL, KEY).unwrap(), None);
        set(&dir, SCOPE_GLOBAL, KEY, &7u32).unwrap();
        assert_eq!(get::<u32>(&dir, SCOPE_GLOBAL, KEY).unwrap(), Some(7));
        let previous = update(&dir, SCOPE_GLOBAL, KEY, |value: &mut u32| {
            let previous = *value;
            *value += 1;
            previous
        })
        .unwrap();
        assert_eq!(previous, 7);
        assert_eq!(get::<u32>(&dir, SCOPE_GLOBAL, KEY).unwrap(), Some(8));
    }

    #[test]
    fn ephemeral_split_routes_by_class() {
        let temp = TempDir::new().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let dir = StateDir::split(
            temp.path().join("volatile"),
            persistent.path().to_path_buf(),
        );

        set(&dir, SCOPE_GLOBAL, KEY, &"kept").unwrap();
        set(&dir, SCOPE_GLOBAL, VOLATILE_KEY, &"gone").unwrap();

        assert_eq!(
            get::<String>(&persistent, SCOPE_GLOBAL, KEY)
                .unwrap()
                .as_deref(),
            Some("kept")
        );
        assert_eq!(
            get::<String>(&persistent, SCOPE_GLOBAL, VOLATILE_KEY).unwrap(),
            None,
            "{EPHEMERAL_SPLIT}"
        );
        assert_eq!(
            get::<String>(&dir, SCOPE_GLOBAL, VOLATILE_KEY)
                .unwrap()
                .as_deref(),
            Some("gone")
        );
    }

    /// Every former standalone file, with the name and lock name it had
    /// before consolidation and a payload the old reader would have accepted.
    const LEGACY_STATE_FILES: [(&str, &str); 11] = [
        ("model", "legacy/model"),
        ("recent-models", "legacy/one\nlegacy/two"),
        ("view", "compact"),
        ("theme", "legacy-theme"),
        ("provider-scripts.json", r#"{"legacy":{"models":[]}}"#),
        ("input_history.json", r#"["legacy prompt"]"#),
        (
            "prompt-stash.json",
            r#"[{"id":"legacy","text":"stashed","created_at":1}]"#,
        ),
        ("prompt-stash.lock", ""),
        ("permission-rules.json", "[]"),
        ("permission-rules.lock", ""),
        ("mcp-trust.json", r#"{"legacy-server":"0123456789abcdef"}"#),
    ];
    const IGNORED: &str = "a file from before consolidation must not be read";
    const UNTOUCHED: &str = "a file from before consolidation must not be rewritten";
    const LEGACY_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn state_files_from_before_consolidation_are_neither_read_nor_written() {
        let (temp, dir) = state_dir();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        for (name, content) in LEGACY_STATE_FILES {
            fs::write(dir.path().join(name), content).unwrap();
        }
        fs::write(
            dir.path().join("permission-config-trust.json"),
            format!(r#"{{"{}":"{LEGACY_DIGEST}"}}"#, project.display()),
        )
        .unwrap();

        assert_eq!(
            crate::model::read_model(&dir, crate::sessions::StoredMode::Build),
            None,
            "{IGNORED}"
        );
        assert!(crate::model::read_recents(&dir).is_empty(), "{IGNORED}");
        assert_eq!(crate::theme::read_theme_name(&dir), None, "{IGNORED}");
        assert_eq!(crate::view::read(&dir), None, "{IGNORED}");
        assert!(
            crate::input_history::InputHistory::load(&dir, 100).is_empty(),
            "{IGNORED}"
        );
        assert!(
            crate::prompt_stash::PromptStash::open(&dir)
                .unwrap()
                .is_empty(),
            "{IGNORED}"
        );
        assert!(
            crate::permission_state::PermissionState::open(&dir)
                .unwrap()
                .records()
                .is_empty(),
            "{IGNORED}"
        );
        assert!(
            !crate::mcp_trust::is_project_trusted(&dir, &project, "legacy-server", LEGACY_DIGEST)
                .unwrap(),
            "{IGNORED}"
        );
        assert!(
            !crate::permission_config_trust::is_project_trusted(&dir, &project, LEGACY_DIGEST)
                .unwrap(),
            "{IGNORED}"
        );

        crate::model::persist_model(&dir, crate::sessions::StoredMode::Build, "current/model");
        crate::theme::persist_theme_name(&dir, "current-theme");
        crate::mcp_trust::trust_project(&dir, &project, "legacy-server", LEGACY_DIGEST).unwrap();
        assert_eq!(
            crate::model::read_model(&dir, crate::sessions::StoredMode::Build).as_deref(),
            Some("current/model")
        );
        for (name, content) in LEGACY_STATE_FILES {
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).unwrap(),
                content,
                "{UNTOUCHED}: {name}"
            );
        }
    }

    #[test]
    fn workspace_tabs_round_trip_per_directory_and_skip_ephemeral() {
        let (temp, dir) = state_dir();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let tabs = WorkspaceTabs {
            open: vec![CaudraId::generate(), CaudraId::generate()],
            focused: None,
        };
        assert_eq!(read_workspace_tabs(&dir, &project).unwrap(), None);
        write_workspace_tabs(&dir, &project, &tabs).unwrap();
        assert_eq!(read_workspace_tabs(&dir, &project).unwrap(), Some(tabs));
        assert_eq!(read_workspace_tabs(&dir, temp.path()).unwrap(), None);

        let ephemeral = StateDir::split(temp.path().join("volatile"), dir.path().to_path_buf());
        write_workspace_tabs(&ephemeral, temp.path(), &WorkspaceTabs::default()).unwrap();
        assert_eq!(read_workspace_tabs(&dir, temp.path()).unwrap(), None);
    }
}
