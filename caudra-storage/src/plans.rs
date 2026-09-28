use std::path::{Path, PathBuf};

use crate::projects::project_subdir;
use crate::words::random_phrase;
use crate::{StateClass, StateDir, StorageError};

pub(crate) const PLANS_DIR: &str = "plans";
const SLUG_RETRIES: usize = 10;

/// Plans live beside the project's other state, keyed by the enclosing
/// repository so a session started in a subdirectory writes to the same place.
pub fn new_plan_path(dir: &StateDir, cwd: &Path) -> Result<PathBuf, StorageError> {
    let plans_dir = dir
        .for_class(StateClass::Persistent)
        .ensure_subdir(project_subdir(cwd).join(PLANS_DIR))?;
    for _ in 0..SLUG_RETRIES {
        let path = plans_dir.join(format!("{}.md", random_phrase()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(StorageError::SlugCollision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateDir;

    #[test]
    fn new_plan_path_under_the_projects_plans_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let cwd = tmp.path().join("workspace");

        let path = new_plan_path(&dir, &cwd).unwrap();

        assert!(path.starts_with(tmp.path().join(project_subdir(&cwd)).join(PLANS_DIR)));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
    }

    #[test]
    fn ephemeral_plans_use_the_persistent_root() {
        let tmp = tempfile::tempdir().unwrap();
        let persistent = tmp.path().join("persistent");
        let volatile = tmp.path().join("volatile");
        let dir = StateDir::split(volatile.clone(), persistent.clone());
        let cwd = tmp.path().join("workspace");

        let path = new_plan_path(&dir, &cwd).unwrap();

        assert!(path.starts_with(persistent.join(project_subdir(&cwd))));
        assert!(!volatile.join(crate::projects::PROJECTS_DIR).exists());
    }

    #[test]
    fn unrelated_projects_get_separate_plan_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        let first = new_plan_path(&dir, &tmp.path().join("alpha")).unwrap();
        let second = new_plan_path(&dir, &tmp.path().join("beta")).unwrap();

        assert_ne!(first.parent(), second.parent());
    }

    #[test]
    fn a_subdirectory_writes_into_the_repositorys_plan_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let root = tmp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(crate::projects::GIT_MARKER)).unwrap();

        let from_root = new_plan_path(&dir, &root).unwrap();
        let from_nested = new_plan_path(&dir, &nested).unwrap();

        assert_eq!(from_root.parent(), from_nested.parent());
    }
}
