use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

use crate::local_documents::{DocumentRevision, LocalDocumentError, revision};
use crate::private_file::{PrivateFile, PrivateFileError, PrivateFileSnapshot};
use crate::projects::project_subdir;
use crate::words::random_phrase;
use crate::{StateDir, StorageError};

pub(crate) const PLANS_DIR: &str = "plans";
const SLUG_RETRIES: usize = 10;
pub const MAX_PLAN_BYTES: usize = 1024 * 1024;

pub struct PlanFile(PrivateFile);

impl PlanFile {
    pub fn new(path: PathBuf) -> Result<Self, LocalDocumentError> {
        Ok(Self(PrivateFile::document(path, MAX_PLAN_BYTES)?))
    }

    pub fn read(&self) -> Result<(String, DocumentRevision), LocalDocumentError> {
        let content = snapshot_content(self.0.load()?)?;
        let revision = revision(&content);
        Ok((content, revision))
    }

    pub fn write(&self, content: &str) -> Result<DocumentRevision, LocalDocumentError> {
        validate_content(content)?;
        let current = self.0.load()?;
        self.0
            .compare_exchange(&current.revision, Some(content.as_bytes()))?;
        Ok(revision(content))
    }

    pub fn replace(
        &self,
        expected: &DocumentRevision,
        content: &str,
    ) -> Result<DocumentRevision, LocalDocumentError> {
        self.rewrite(expected, |_| Ok(content.to_owned()))
    }

    pub(crate) fn rewrite(
        &self,
        expected: &DocumentRevision,
        edit: impl FnOnce(String) -> Result<String, LocalDocumentError>,
    ) -> Result<DocumentRevision, LocalDocumentError> {
        let current = self.0.load()?;
        let file_revision = current.revision.clone();
        let content = snapshot_content(current)?;
        if revision(&content) != *expected {
            return Err(LocalDocumentError::StaleRevision {
                expected: expected.as_str().to_owned(),
            });
        }
        let content = edit(content)?;
        validate_content(&content)?;
        self.0
            .compare_exchange(&file_revision, Some(content.as_bytes()))
            .map_err(|error| match error {
                PrivateFileError::Conflict => LocalDocumentError::StaleRevision {
                    expected: expected.as_str().to_owned(),
                },
                other => other.into(),
            })?;
        Ok(revision(&content))
    }
}

pub fn validate_content(content: &str) -> Result<(), LocalDocumentError> {
    if content.len() > MAX_PLAN_BYTES {
        Err(LocalDocumentError::TooLarge)
    } else {
        Ok(())
    }
}

fn snapshot_content(snapshot: PrivateFileSnapshot) -> Result<String, LocalDocumentError> {
    String::from_utf8(snapshot.data.unwrap_or_default())
        .map_err(|error| LocalDocumentError::Io(Error::new(ErrorKind::InvalidData, error)))
}

/// Plans live beside the project's other state, keyed by the enclosing
/// repository so a session started in a subdirectory writes to the same place.
pub fn new_plan_path(dir: &StateDir, cwd: &Path) -> Result<PathBuf, StorageError> {
    let plans_dir = dir
        .persistent_path()
        .join(project_subdir(cwd))
        .join(PLANS_DIR);
    for _ in 0..SLUG_RETRIES {
        let path = plans_dir.join(format!("{}.md", random_phrase()));
        if !path.exists() {
            PrivateFile::document(path.clone(), MAX_PLAN_BYTES)
                .and_then(|file| file.ensure_parent())
                .map_err(Error::other)?;
            return Ok(path);
        }
    }
    Err(StorageError::SlugCollision)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{MAX_PLAN_BYTES, PLANS_DIR, PlanFile, new_plan_path, project_subdir};
    use crate::StateDir;
    use crate::local_documents::LocalDocumentError;
    use crate::private_file::PrivateFileError;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    const CONTENT: &str = "# Plan\nComplete the work.";
    const UPDATED: &str = "# Plan\nVerify the work.";
    const FILE_NAME: &str = "plan.md";
    #[cfg(unix)]
    const PUBLIC_READ_MODE: u32 = 0o644;
    #[cfg(unix)]
    const SHARED_WRITE_MODE: u32 = 0o666;
    #[cfg(unix)]
    const PRIVATE_MODE: u32 = 0o600;
    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;
    #[cfg(unix)]
    const DIRECTORY_MODE: u32 = 0o700;

    pub(crate) fn tempdir() -> TempDir {
        let mut builder = Builder::new();
        #[cfg(unix)]
        builder.permissions(fs::Permissions::from_mode(DIRECTORY_MODE));
        builder
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    #[test_case(false; "missing_target")]
    #[test_case(true; "missing_parent")]
    fn local_plan_round_trip_and_revision_conflict(missing_parent: bool) {
        let root = tempdir();
        let directory = if missing_parent {
            root.path().join("new")
        } else {
            root.path().to_path_buf()
        };
        let path = directory.join(FILE_NAME);
        let file = PlanFile::new(path.clone()).unwrap();
        assert!(file.read().unwrap().0.is_empty());
        assert!(!path.exists());
        assert_eq!(directory.exists(), !missing_parent);
        let first = file.write(CONTENT).unwrap();
        let (content, revision) = file.read().unwrap();
        assert_eq!(content, CONTENT);
        assert_eq!(revision, first);
        let second = file.replace(&first, UPDATED).unwrap();
        assert_ne!(first, second);
        assert!(matches!(
            file.replace(&first, CONTENT),
            Err(LocalDocumentError::StaleRevision { .. })
        ));
        assert_eq!(file.read().unwrap(), (UPDATED.to_owned(), second));
    }

    #[test_case(false; "write")]
    #[test_case(true; "read")]
    fn local_plan_rejects_oversize_content(read: bool) {
        let root = tempdir();
        let path = root.path().join(FILE_NAME);
        let file = PlanFile::new(path.clone()).unwrap();
        let oversize = "x".repeat(MAX_PLAN_BYTES + 1);
        if read {
            file.write("").unwrap();
            fs::write(path, oversize).unwrap();
            assert!(matches!(
                file.read(),
                Err(LocalDocumentError::PrivateFile(PrivateFileError::TooLarge))
            ));
        } else {
            assert!(matches!(
                file.write(&oversize),
                Err(LocalDocumentError::TooLarge)
            ));
            assert!(!path.exists());
        }
    }

    #[cfg(unix)]
    #[test_case(PUBLIC_READ_MODE, true; "legacy_public_read")]
    #[test_case(SHARED_WRITE_MODE, false; "shared_write_refused")]
    fn local_plan_preflight_preserves_permissions(mode: u32, allowed: bool) {
        let root = tempdir();
        let path = root.path().join(FILE_NAME);
        fs::write(&path, CONTENT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let file = PlanFile::new(path.clone()).unwrap();
        assert_eq!(file.read().is_ok(), allowed);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & MODE_MASK,
            mode
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(file.write(UPDATED).is_ok(), allowed);
        if allowed {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & MODE_MASK,
                PRIVATE_MODE
            );
        }
    }

    #[cfg(unix)]
    #[test_case(false; "file_symlink")]
    #[test_case(true; "ancestor_symlink")]
    fn local_plan_refuses_symlinks(ancestor: bool) {
        let root = tempdir();
        let path = root.path().join(FILE_NAME);
        fs::write(&path, CONTENT).unwrap();
        let link = root.path().join("link");
        let target = if ancestor {
            symlink(root.path(), &link).unwrap();
            link.join(FILE_NAME)
        } else {
            symlink(&path, &link).unwrap();
            link
        };
        let file = PlanFile::new(target).unwrap();
        assert!(matches!(
            file.read(),
            Err(LocalDocumentError::PrivateFile(
                PrivateFileError::UnsafePath
            ))
        ));
        assert!(matches!(
            file.write(UPDATED),
            Err(LocalDocumentError::PrivateFile(
                PrivateFileError::UnsafePath
            ))
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), CONTENT);
    }

    #[test]
    fn new_plan_path_under_the_projects_plans_dir() {
        let tmp = tempdir();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let cwd = tmp.path().join("workspace");

        let path = new_plan_path(&dir, &cwd).unwrap();

        assert!(path.starts_with(tmp.path().join(project_subdir(&cwd)).join(PLANS_DIR)));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
    }

    #[test]
    fn ephemeral_plans_use_the_persistent_root() {
        let tmp = tempdir();
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
        let tmp = tempdir();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        let first = new_plan_path(&dir, &tmp.path().join("alpha")).unwrap();
        let second = new_plan_path(&dir, &tmp.path().join("beta")).unwrap();

        assert_ne!(first.parent(), second.parent());
    }

    #[test]
    fn a_subdirectory_writes_into_the_repositorys_plan_dir() {
        let tmp = tempdir();
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
