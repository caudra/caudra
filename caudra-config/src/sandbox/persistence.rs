use super::{MAX_SANDBOX_FILE_BYTES, SANDBOX_FILE, SandboxDraft, SandboxError, SavedSandboxes};
use caudra_storage::paths;
use caudra_storage::private_file::{FileRevision, PrivateFile, PrivateFileError};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxStoreError {
    #[error("could not resolve the client configuration directory")]
    ConfigDirectory,
    #[error("sandbox baseline belongs to a different store; load this store before saving")]
    WrongStore,
    #[error(transparent)]
    File(#[from] PrivateFileError),
    #[error(transparent)]
    Configuration(#[from] SandboxError),
}

pub struct SandboxStore(PrivateFile);

pub struct LoadedSandboxes {
    path: PathBuf,
    file_revision: FileRevision,
    saved: SavedSandboxes,
    document: DocumentMut,
}

impl fmt::Debug for LoadedSandboxes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedSandboxes")
            .field("file_revision", &self.file_revision)
            .field("revision", self.saved.revision())
            .finish_non_exhaustive()
    }
}

impl LoadedSandboxes {
    pub fn saved(&self) -> &SavedSandboxes {
        &self.saved
    }
    pub fn draft(&self) -> SandboxDraft {
        self.saved.draft()
    }
    pub fn file_revision(&self) -> &FileRevision {
        &self.file_revision
    }

    pub fn with_imported_format(&self, source: &str) -> Result<Self, SandboxStoreError> {
        Ok(Self {
            path: self.path.clone(),
            file_revision: self.file_revision.clone(),
            saved: SavedSandboxes::new(SandboxDraft::import(source)?)?,
            document: source
                .parse::<DocumentMut>()
                .map_err(|_| SandboxError::Document)?,
        })
    }

    pub fn export_draft(&self, draft: &SandboxDraft) -> Result<String, SandboxStoreError> {
        let desired = draft
            .export()?
            .parse::<DocumentMut>()
            .map_err(|_| SandboxError::Document)?;
        let previous = self
            .saved
            .configuration()
            .export()?
            .parse::<DocumentMut>()
            .map_err(|_| SandboxError::Document)?;
        let mut document = self.document.clone();
        if document.as_table().is_empty() {
            document = desired;
        } else {
            merge_table(
                document.as_table_mut(),
                previous.as_table(),
                desired.as_table(),
            );
        }
        let source = document.to_string();
        if SandboxDraft::import(&source)? != SandboxDraft::import(&draft.export()?)? {
            return Err(SandboxError::Document.into());
        }
        Ok(source)
    }
}

impl SandboxStore {
    pub fn user_global() -> Result<Self, SandboxStoreError> {
        Self::from_config_dir(
            &paths::config_dir_path().map_err(|_| SandboxStoreError::ConfigDirectory)?,
        )
    }

    /// For an explicitly trusted client directory; never pass a remote/project config root.
    pub fn from_config_dir(directory: &Path) -> Result<Self, SandboxStoreError> {
        Ok(Self(PrivateFile::new(
            directory.join(SANDBOX_FILE),
            MAX_SANDBOX_FILE_BYTES,
        )?))
    }

    pub fn load(&self) -> Result<LoadedSandboxes, SandboxStoreError> {
        let snapshot = self.0.load()?;
        let (draft, document) = if let Some(bytes) = snapshot.data {
            let source = String::from_utf8(bytes).map_err(|_| SandboxError::Document)?;
            let draft = SandboxDraft::import(&source)?;
            (
                draft,
                source
                    .parse::<DocumentMut>()
                    .map_err(|_| SandboxError::Document)?,
            )
        } else {
            (SandboxDraft::new(), DocumentMut::new())
        };
        Ok(LoadedSandboxes {
            path: self.0.path().into(),
            file_revision: snapshot.revision,
            saved: SavedSandboxes::new(draft)?,
            document,
        })
    }

    pub fn save(
        &self,
        baseline: &LoadedSandboxes,
        draft: &SandboxDraft,
    ) -> Result<LoadedSandboxes, SandboxStoreError> {
        if baseline.path != self.0.path() {
            return Err(SandboxStoreError::WrongStore);
        }
        let saved = SavedSandboxes::new(draft.clone())?;
        let source = baseline.export_draft(saved.configuration())?;
        let document = source
            .parse::<DocumentMut>()
            .map_err(|_| SandboxError::Document)?;
        let file_revision = self
            .0
            .compare_exchange(&baseline.file_revision, Some(source.as_bytes()))?;
        Ok(LoadedSandboxes {
            path: baseline.path.clone(),
            file_revision,
            saved,
            document,
        })
    }
}

fn merge_table(current: &mut Table, previous: &Table, desired: &Table) {
    current.retain(|key, _| desired.contains_key(key));
    for (key, value) in desired {
        if previous
            .get(key)
            .is_some_and(|before| same_item(before, value))
        {
            continue;
        }
        match (current.get_mut(key), value) {
            (Some(Item::Table(current)), Item::Table(desired)) => {
                merge_table(
                    current,
                    previous
                        .get(key)
                        .and_then(Item::as_table)
                        .unwrap_or(&Table::new()),
                    desired,
                );
            }
            (Some(Item::Value(current)), Item::Value(desired)) => {
                let decor = current.decor().clone();
                *current = desired.clone();
                *current.decor_mut() = decor;
            }
            _ => {
                current.insert(key, value.clone());
            }
        }
    }
}

fn same_item(left: &Item, right: &Item) -> bool {
    match (left, right) {
        (Item::Table(left), Item::Table(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| same_item(value, other)))
        }
        (Item::Value(left), Item::Value(right)) => left.to_string() == right.to_string(),
        _ => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{SandboxStore, SandboxStoreError};
    use crate::sandbox::tests::{SAMPLE, draft, name};
    use crate::sandbox::{
        MAX_SANDBOX_FILE_BYTES, RecordKind, SANDBOX_FILE, SandboxDraft, SandboxError,
    };
    use crate::workcell::{WorkcellSelection, load_workcell_profiles_from, select_workcell};
    use caudra_storage::private_file::{FileRevision, PrivateFileError};
    use std::env;
    use std::fs::{self, Permissions};
    use std::io;
    use std::num::NonZeroU32;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    const FILE_MODE: u32 = 0o600;
    const DIRECTORY_MODE: u32 = 0o700;
    const GLOBAL_PATH_TEST: &str = "CAUDRA_SANDBOX_GLOBAL_PATH_TEST";
    const GLOBAL_PATH_TEST_NAME: &str = "sandbox::persistence::tests::user_global_load_is_inert";
    const CHANGED_COMMENT: &str = "# external edit\n";
    const CHANGED_CPU_LINE: &str = "cpus = 6 # virtual CPUs";
    const UNRELATED_PROVIDER: &str = "[sandbox.providers.local]\nkind = \"e2b-libvirt\"\napi_endpoint = \"http://127.0.0.1:3000\"\nproxy_endpoint = \"http://127.0.0.1:49983\"\ncredential_ref = \"sandbox-api:local\"";

    fn tempdir() -> io::Result<TempDir> {
        Builder::new()
            .permissions(Permissions::from_mode(DIRECTORY_MODE))
            .tempdir()
    }

    fn write(directory: &Path, text: &str) {
        let path = directory.join(SANDBOX_FILE);
        fs::write(&path, text).unwrap();
        fs::set_permissions(path, Permissions::from_mode(FILE_MODE)).unwrap();
    }

    #[test]
    fn missing_file_leaves_embedded_mode_and_disk_unchanged() {
        let temp = tempdir().unwrap();
        let directory = temp.path().join("not-created");
        let store = SandboxStore::from_config_dir(&directory).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.file_revision(), &FileRevision::Missing);
        assert_eq!(loaded.draft(), SandboxDraft::new());
        let workcell = load_workcell_profiles_from(&directory).unwrap();
        assert_eq!(
            select_workcell(&workcell, None, None, None, None).unwrap(),
            WorkcellSelection::Embedded
        );
        assert!(!directory.exists());
    }

    #[test]
    fn user_global_load_is_inert() {
        if let Some(directory) = env::var_os(GLOBAL_PATH_TEST) {
            let directory = PathBuf::from(directory);
            let store = SandboxStore::user_global().unwrap();
            assert!(store.0.path().starts_with(&directory));
            assert_eq!(
                store.load().unwrap().file_revision(),
                &FileRevision::Missing
            );
            assert!(!directory.exists());
            return;
        }
        let temp = tempdir().unwrap();
        let directory = temp.path().join("unused-config");
        let status = Command::new(env::current_exe().unwrap())
            .args(["--exact", GLOBAL_PATH_TEST_NAME])
            .env(GLOBAL_PATH_TEST, &directory)
            .env("XDG_CONFIG_HOME", &directory)
            .env_remove("CAUDRA_NAMESPACE")
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!directory.exists());
    }

    #[test_case(RecordKind::Provider, "local")]
    #[test_case(RecordKind::Network, "build")]
    #[test_case(RecordKind::Transfer, "source")]
    #[test_case(RecordKind::Profile, "dev")]
    fn persists_duplicate_and_delete_without_touching_other_records(
        kind: RecordKind,
        source: &str,
    ) {
        let temp = tempdir().unwrap();
        let store = SandboxStore::from_config_dir(temp.path()).unwrap();
        let baseline = store.save(&store.load().unwrap(), &draft()).unwrap();
        let mut edited = baseline.draft();
        let copy = name("duplicate");
        edited
            .duplicate(kind.clone(), &name(source), copy.clone())
            .unwrap();
        let added = store.save(&baseline, &edited).unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .draft()
                .get(kind.clone(), &copy)
                .unwrap(),
            edited.get(kind.clone(), &name(source)).unwrap()
        );
        edited.remove(kind, &copy).unwrap();
        let removed = store.save(&added, &edited).unwrap();
        assert_eq!(removed.saved(), baseline.saved());
    }

    #[test]
    fn saves_revisions_and_preserves_comments_and_unrelated_sections() {
        let temp = tempdir().unwrap();
        write(temp.path(), SAMPLE);
        let store = SandboxStore::from_config_dir(temp.path()).unwrap();
        let loaded = store.load().unwrap();
        let unchanged = store.save(&loaded, &loaded.draft()).unwrap();
        assert_eq!(unchanged.file_revision(), loaded.file_revision());
        assert_eq!(
            fs::read_to_string(temp.path().join(SANDBOX_FILE)).unwrap(),
            SAMPLE
        );
        let mut draft = loaded.draft();
        draft.profiles.get_mut(&name("dev")).unwrap().cpus = NonZeroU32::new(6).unwrap();
        let committed = store.save(&loaded, &draft).unwrap();
        assert_ne!(committed.saved().revision(), loaded.saved().revision());
        assert_eq!(store.load().unwrap().saved(), committed.saved());
        let source = fs::read_to_string(temp.path().join(SANDBOX_FILE)).unwrap();
        assert!(source.starts_with("# Client-owned launch defaults"));
        assert!(source.contains(CHANGED_CPU_LINE));
        assert!(source.contains("# reviewed hosts"));
        assert!(source.contains("# Shared policy"));
        assert!(source.contains(UNRELATED_PROVIDER));
        let again = store.save(&committed, &committed.draft()).unwrap();
        assert_eq!(again.file_revision(), committed.file_revision());
        assert_eq!(
            fs::metadata(temp.path().join(SANDBOX_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            FILE_MODE
        );
        let exported = committed.draft().export().unwrap();
        assert_eq!(SandboxDraft::import(&exported).unwrap(), draft);
    }

    #[test_case(false; "comment_only_edit")]
    #[test_case(true; "removed_file")]
    fn compare_and_swap_rejects_external_edits_without_clobbering(removed: bool) {
        let temp = tempdir().unwrap();
        write(temp.path(), SAMPLE);
        let store = SandboxStore::from_config_dir(temp.path()).unwrap();
        let loaded = store.load().unwrap();
        let path = temp.path().join(SANDBOX_FILE);
        if removed {
            fs::remove_file(&path).unwrap();
        } else {
            write(temp.path(), &format!("{CHANGED_COMMENT}{SAMPLE}"));
        }
        let bytes_before = fs::read(&path).ok();
        let error = store.save(&loaded, &loaded.draft()).unwrap_err();
        assert_eq!(error, SandboxStoreError::File(PrivateFileError::Conflict));
        assert_eq!(fs::read(&path).ok(), bytes_before);
        if !removed {
            assert_eq!(
                store.load().unwrap().saved().revision(),
                loaded.saved().revision()
            );
        }
    }

    #[test]
    fn new_file_creation_conflicts_and_drafts_are_not_committed_on_failure() {
        let temp = tempdir().unwrap();
        let store = SandboxStore::from_config_dir(temp.path()).unwrap();
        let baseline = store.load().unwrap();
        let committed = store.save(&baseline, &draft()).unwrap();
        assert_eq!(
            store.save(&baseline, &SandboxDraft::new()).unwrap_err(),
            SandboxStoreError::File(PrivateFileError::Conflict)
        );
        let before = fs::read(temp.path().join(SANDBOX_FILE)).unwrap();
        let mut broken = committed.draft();
        broken.remove(RecordKind::Network, &name("build")).unwrap();
        assert!(matches!(
            store.save(&committed, &broken),
            Err(SandboxStoreError::Configuration(
                SandboxError::Dangling { .. }
            ))
        ));
        assert_eq!(fs::read(temp.path().join(SANDBOX_FILE)).unwrap(), before);
        let elsewhere = tempdir().unwrap();
        let other = SandboxStore::from_config_dir(elsewhere.path()).unwrap();
        assert_eq!(
            other.save(&committed, &draft()).unwrap_err(),
            SandboxStoreError::WrongStore
        );
        assert!(!elsewhere.path().join(SANDBOX_FILE).exists());
    }

    #[test_case(false; "file_symlink")]
    #[test_case(true; "directory_symlink")]
    fn store_never_follows_symlinks(directory_link: bool) {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        write(&target, SAMPLE);
        let link = temp.path().join("link");
        let store = if directory_link {
            symlink(&target, &link).unwrap();
            SandboxStore::from_config_dir(&link).unwrap()
        } else {
            symlink(target.join(SANDBOX_FILE), temp.path().join(SANDBOX_FILE)).unwrap();
            SandboxStore::from_config_dir(temp.path()).unwrap()
        };
        assert_eq!(
            store.load().unwrap_err(),
            SandboxStoreError::File(PrivateFileError::UnsafePath)
        );
        assert_eq!(
            fs::read_to_string(target.join(SANDBOX_FILE)).unwrap(),
            SAMPLE
        );
    }

    #[test]
    fn malformed_permissions_and_oversized_files_do_not_fall_back_to_empty() {
        let temp = tempdir().unwrap();
        let store = SandboxStore::from_config_dir(temp.path()).unwrap();
        write(temp.path(), SAMPLE);
        fs::set_permissions(
            temp.path().join(SANDBOX_FILE),
            Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            store.load().unwrap_err(),
            SandboxStoreError::File(PrivateFileError::Permissions)
        );
        write(temp.path(), &"x".repeat(MAX_SANDBOX_FILE_BYTES + 1));
        assert_eq!(
            store.load().unwrap_err(),
            SandboxStoreError::File(PrivateFileError::TooLarge)
        );
        write(temp.path(), "token = 'secret-not-allowed'");
        let error = store.load().unwrap_err();
        assert!(matches!(
            error,
            SandboxStoreError::Configuration(SandboxError::Parse { .. })
        ));
        assert!(!format!("{error:?}: {error}").contains("secret-not-allowed"));
    }
}
