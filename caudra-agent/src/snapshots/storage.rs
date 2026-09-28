//! The store on disk: pointer files, restore records, the directories holding
//! them, and the listing `caudra storage snapshots` and `/storage` report.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};
use workcell::snapshot_store::{ObjectStore, SnapshotId, StoreOptions};

use super::{
    SESSION_SNAPSHOTS_DIR, SnapshotError, SnapshotKey, SnapshotStore, WORKSPACE_SNAPSHOTS_DIR,
    canonical_root,
};

pub(super) const START_POINTER: &str = "start";
pub(super) const JOURNAL_NAME: &str = "restore-journal.json";
pub(super) const UNREVERT_NAME: &str = "unrevert.json";
const CHECKPOINTS_DIR: &str = "checkpoints";
const WORKSPACE_ROOT_NAME: &str = "workspace-root.json";
pub(super) const OBJECTS_DIR: &str = "objects";
const REPOSITORY_OPTIONS: StoreOptions = StoreOptions {
    private: false,
    max_object_bytes: None,
};

/// One workspace store on disk, as `caudra storage snapshots` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct StoreEntry {
    pub workspace_key: String,
    /// The worktree this store captures, or `None` when its binding is gone
    /// and the store is orphaned.
    pub root: Option<PathBuf>,
    /// The shared objects plus every session's pointers and restore records.
    pub bytes: u64,
    pub objects: u64,
    pub sessions: Vec<SessionSnapshots>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshots {
    pub session_id: String,
    /// `start` and each checkpoint id, as the session's pointer files name them.
    pub snapshots: Vec<String>,
}

impl StoreEntry {
    pub fn snapshot_count(&self) -> usize {
        self.sessions
            .iter()
            .map(|session| session.snapshots.len())
            .sum()
    }
}

impl SnapshotStore {
    /// Every workspace store under `root`, largest first.
    ///
    /// Read from the layout rather than an index, because the store is the
    /// authority on its own size. A store whose binding is gone, or that no
    /// session names any more, still reports what it costs: an orphaned store
    /// is exactly what an operator is looking for.
    pub fn store_entries(root: &Path) -> Vec<StoreEntry> {
        let mut stores: BTreeMap<String, StoreEntry> = BTreeMap::new();
        for (key, repository) in
            subdirectories(&root.join(WORKSPACE_SNAPSHOTS_DIR)).unwrap_or_default()
        {
            let (object_bytes, objects) = tree_totals(&repository.join(OBJECTS_DIR));
            let store = stores
                .entry(key.clone())
                .or_insert_with(|| empty_entry(key));
            store.root = read_json(&repository.join(WORKSPACE_ROOT_NAME), WORKSPACE_ROOT_NAME).ok();
            store.bytes += object_bytes + shallow_bytes(&repository);
            store.objects = objects;
        }
        for (session_id, session) in
            subdirectories(&root.join(SESSION_SNAPSHOTS_DIR)).unwrap_or_default()
        {
            for (key, dir) in subdirectories(&session).unwrap_or_default() {
                let store = stores
                    .entry(key.clone())
                    .or_insert_with(|| empty_entry(key));
                store.bytes += tree_totals(&dir).0;
                store.sessions.push(SessionSnapshots {
                    session_id: session_id.clone(),
                    snapshots: pointer_names(&dir),
                });
            }
        }
        let mut stores: Vec<StoreEntry> = stores.into_values().collect();
        for store in &mut stores {
            store
                .sessions
                .sort_by(|left, right| left.session_id.cmp(&right.session_id));
        }
        stores.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.workspace_key.cmp(&right.workspace_key))
        });
        stores
    }

    /// Whether this session's directory holds a store from before snapshots
    /// were git objects. Its manifests, journal and unrevert record mean
    /// nothing here.
    pub fn is_legacy(&self) -> bool {
        is_legacy(&self.dir)
    }

    /// Names the same snapshots from `destination`: the session start and each
    /// of `checkpoints` this store has. Only pointers are written, because the
    /// objects already belong to the workspace both sessions share.
    pub fn copy_ancestry_to(
        &self,
        destination: &SnapshotStore,
        checkpoints: &[CaudraId],
    ) -> Result<(), SnapshotError> {
        if destination.repository != self.repository {
            return Err(SnapshotError::ForeignWorkspace);
        }
        let _lock = self.lock()?;
        let mut pointers = Vec::new();
        for key in [SnapshotKey::SessionStart]
            .into_iter()
            .chain(checkpoints.iter().copied().map(SnapshotKey::Checkpoint))
        {
            if let Some(id) = self.pointer(key)? {
                pointers.push((key, id));
            }
        }
        if pointers.is_empty() {
            return Ok(());
        }
        destination.ensure_dirs()?;
        pointers
            .into_iter()
            .try_for_each(|(key, id)| destination.write_pointer(key, id))
    }

    pub(super) fn pointer_path(&self, key: SnapshotKey) -> PathBuf {
        match key {
            SnapshotKey::SessionStart => self.dir.join(START_POINTER),
            SnapshotKey::Checkpoint(checkpoint) => {
                checkpoints_dir(&self.dir).join(checkpoint.to_string())
            }
        }
    }

    pub(super) fn pointer(&self, key: SnapshotKey) -> Result<Option<SnapshotId>, SnapshotError> {
        read_pointer(&self.pointer_path(key))
    }

    pub(super) fn write_pointer(
        &self,
        key: SnapshotKey,
        id: SnapshotId,
    ) -> Result<(), SnapshotError> {
        write_atomic(&self.pointer_path(key), id.to_string().as_bytes())
    }

    pub(super) fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_NAME)
    }

    pub(super) fn unrevert_path(&self) -> PathBuf {
        self.dir.join(UNREVERT_NAME)
    }

    /// Where every session's directory for this workspace lives, as
    /// `<sessions>/<session>/<key>`.
    pub(super) fn sessions_dir(&self) -> &Path {
        self.dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.dir)
    }

    pub(super) fn workspace_key(&self) -> &str {
        self.dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }

    pub(super) fn open_repository(&self) -> Result<ObjectStore, SnapshotError> {
        open_repository(&self.repository)
    }

    /// Creates this session's directory and the workspace store, replacing a
    /// store left in the pre-git format.
    pub(super) fn ensure_dirs(&self) -> Result<(), SnapshotError> {
        self.create_directory(&self.dir)?;
        if self.is_legacy() {
            fs::remove_dir_all(&self.dir)?;
            info!(dir = %self.dir.display(), "removed a snapshot store in the pre-git format");
        }
        self.create_directory(&checkpoints_dir(&self.dir))?;
        self.create_directory(&self.repository)
    }

    fn create_directory(&self, dir: &Path) -> Result<(), SnapshotError> {
        match &self.artifact_state {
            Some(state_dir) => create_managed_directory(state_dir, dir),
            None => Ok(fs::create_dir_all(dir)?),
        }
    }

    /// Ensures the store exists and captures `cwd` alone: the object store of
    /// one workspace must never take another's files.
    pub(super) fn bind_root(&self, cwd: &Path) -> Result<PathBuf, SnapshotError> {
        self.ensure_dirs()?;
        let root = canonical_root(cwd)?;
        let binding = self.repository.join(WORKSPACE_ROOT_NAME);
        match read_json::<PathBuf>(&binding, WORKSPACE_ROOT_NAME) {
            Ok(expected) if expected == root => Ok(root),
            Ok(expected) => Err(SnapshotError::WorkspaceRootMismatch {
                expected,
                actual: root,
            }),
            Err(SnapshotError::NotFound(_)) => {
                write_json(&binding, &root)?;
                Ok(root)
            }
            Err(error) => Err(error),
        }
    }
}

fn empty_entry(workspace_key: String) -> StoreEntry {
    StoreEntry {
        workspace_key,
        root: None,
        bytes: 0,
        objects: 0,
        sessions: Vec::new(),
    }
}

pub(super) fn open_repository(dir: &Path) -> Result<ObjectStore, SnapshotError> {
    Ok(ObjectStore::open(dir, REPOSITORY_OPTIONS)?)
}

/// Deletes what `repository` staged for a write that failed, as dropping it
/// would, but without losing a failure to delete, and passes `result` on.
pub(super) fn abandon_on_error<T>(
    repository: &ObjectStore,
    result: Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    if result.is_err()
        && let Err(error) = repository.abandon()
    {
        warn!(
            store = %repository.dir().display(),
            %error,
            "could not delete staged snapshot objects; the next collection will"
        );
    }
    result
}

pub(super) fn checkpoints_dir(session_dir: &Path) -> PathBuf {
    session_dir.join(CHECKPOINTS_DIR)
}

pub(super) fn is_legacy(session_dir: &Path) -> bool {
    session_dir.join(OBJECTS_DIR).is_dir()
}

pub(super) fn read_pointer(path: &Path) -> Result<Option<SnapshotId>, SnapshotError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    text.trim()
        .parse()
        .map(Some)
        .map_err(|_| SnapshotError::InvalidPointer(path.to_path_buf()))
}

/// The entries directly in `dir`, and none when it is absent. Any other
/// failure is an error: garbage collection that read an unlistable directory
/// as empty would free every object the pointers in it name.
fn dir_entries(dir: &Path) -> Result<Vec<fs::DirEntry>, SnapshotError> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(entries.collect::<io::Result<_>>()?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// The directories directly in `dir`, by name. A name that is not UTF-8 was
/// never written by Caudra, whose session ids and workspace keys are ASCII.
pub(super) fn subdirectories(dir: &Path) -> Result<Vec<(String, PathBuf)>, SnapshotError> {
    let mut directories = Vec::new();
    for entry in dir_entries(dir)? {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string() {
            directories.push((name, entry.path()));
        }
    }
    Ok(directories)
}

/// Each checkpoint pointer in a session directory, oldest first: checkpoint
/// ids are time-ordered.
pub(super) fn checkpoint_pointers(
    session_dir: &Path,
) -> Result<Vec<(CaudraId, PathBuf)>, SnapshotError> {
    let mut pointers: Vec<(CaudraId, PathBuf)> = dir_entries(&checkpoints_dir(session_dir))?
        .into_iter()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
        .collect();
    pointers.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    Ok(pointers)
}

fn pointer_names(session_dir: &Path) -> Vec<String> {
    session_dir
        .join(START_POINTER)
        .exists()
        .then(|| START_POINTER.to_owned())
        .into_iter()
        .chain(
            checkpoint_pointers(session_dir)
                .unwrap_or_default()
                .into_iter()
                .map(|(checkpoint, _)| checkpoint.to_string()),
        )
        .collect()
}

/// Bytes and file count under `dir`, in one pass. A store holds an object per
/// distinct file version its workspace ever had, so on a large workspace this
/// walk is the expensive part of reporting.
fn tree_totals(dir: &Path) -> (u64, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .fold((0, 0), |(bytes, files), entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                let (sub_bytes, sub_files) = tree_totals(&entry.path());
                (bytes + sub_bytes, files + sub_files)
            }
            Ok(_) => (
                bytes + entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                files + 1,
            ),
            Err(_) => (bytes, files),
        })
}

/// Bytes of the files sitting directly in `dir`: everything a store keeps
/// beside `objects/`, without descending that tree a second time.
fn shallow_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

pub(super) fn read_json<T: DeserializeOwned>(path: &Path, name: &str) -> Result<T, SnapshotError> {
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            SnapshotError::NotFound(name.to_owned())
        } else {
            SnapshotError::Io(error)
        }
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// `read_json`, with an absent file as `None`.
pub(super) fn read_optional_json<T: DeserializeOwned>(
    path: &Path,
    name: &str,
) -> Result<Option<T>, SnapshotError> {
    match read_json(path, name) {
        Ok(value) => Ok(Some(value)),
        Err(SnapshotError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn write_json(path: &Path, value: &impl Serialize) -> Result<(), SnapshotError> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    caudra_storage::atomic_write(path, bytes)
        .map_err(|error| io::Error::other(error.to_string()).into())
}

pub(super) fn remove_file_durable(path: &Path) -> Result<(), SnapshotError> {
    fs::remove_file(path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// `remove_file_durable`, with a file that is already gone as success.
pub(super) fn remove_if_present(path: &Path) -> Result<(), SnapshotError> {
    match remove_file_durable(path) {
        Err(SnapshotError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn create_managed_directory(state_dir: &StateDir, target: &Path) -> Result<(), SnapshotError> {
    let relative = target.strip_prefix(state_dir.path()).map_err(|_| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed snapshot path is outside the state directory",
        ))
    })?;
    let mut path = state_dir.path().to_path_buf();
    validate_or_create_directory(&path, false)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(SnapshotError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed snapshot path is not normalized",
            )));
        };
        path.push(component);
        validate_or_create_directory(&path, true)?;
    }
    Ok(())
}

fn validate_or_create_directory(path: &Path, create: bool) -> Result<(), SnapshotError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir(path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                fs::File::open(parent)?.sync_all()?;
            }
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SnapshotError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("snapshot path {} is not a real directory", path.display()),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::snapshots::restore::restore_objects;
    use crate::snapshots::tests::{
        ALPHA, BETA, KEY, OTHER_KEY, OTHER_SESSION, SESSION, id, object_count, paths, read,
        session_store, setup, state_root, store_in, write,
    };

    const FORK_MSG: &str = "a fork names the parent's snapshots without copying a single object";
    const FOREIGN_MSG: &str = "snapshots of one workspace cannot be named from another";
    const LEGACY_MSG: &str =
        "a store in the pre-git format reads as empty and is replaced when next written";
    const LISTING_MSG: &str =
        "the listing reports each workspace with the snapshots every session names";
    const LEGACY_RECORD: &str = r#"{"state":"prepare","paths":["file.txt"]}"#;
    const LEGACY_MANIFEST: &str = "session-start.json";

    #[test]
    fn a_fork_names_the_same_snapshots_without_copying_objects() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let parent = store_in(&temp);
        parent.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let included = id(1);
        let omitted = id(2);
        parent.snapshot(&root, included).unwrap();
        write(&root, "file.txt", "current");
        parent.snapshot(&root, omitted).unwrap();
        let objects = object_count(&parent);
        let child = session_store(&temp, OTHER_SESSION);

        parent.copy_ancestry_to(&child, &[included]).unwrap();

        for key in [SnapshotKey::SessionStart, SnapshotKey::Checkpoint(included)] {
            assert_eq!(
                child.snapshot_id(key).unwrap(),
                parent.snapshot_id(key).unwrap(),
                "{FORK_MSG}"
            );
        }
        assert!(!child.has_checkpoint(omitted), "{FORK_MSG}");
        assert_eq!(object_count(&child), objects, "{FORK_MSG}");
        assert_eq!(read(&root, "file.txt"), "current", "{FORK_MSG}");
    }

    #[test]
    fn a_fork_into_another_workspace_is_refused() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let parent = store_in(&temp);
        parent.snapshot_session_start(&root).unwrap();
        let foreign = SnapshotStore::new(&state_root(&temp), id(OTHER_SESSION), OTHER_KEY);

        let error = parent.copy_ancestry_to(&foreign, &[]).unwrap_err();

        assert!(
            matches!(error, SnapshotError::ForeignWorkspace),
            "{FOREIGN_MSG}: {error:?}"
        );
        assert!(!foreign.has_session_start(), "{FOREIGN_MSG}");
    }

    /// The binding belongs to the workspace store, so it holds for every
    /// session that shares it, not only the one that made it.
    #[test]
    fn a_store_refuses_to_capture_restore_or_recover_another_root() {
        let (temp, root) = setup();
        let other_root = temp.path().join("other-repo");
        fs::create_dir(&other_root).unwrap();
        write(&root, "file.txt", ALPHA);
        write(&other_root, "file.txt", "other");
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let reopened = store_in(&temp);

        let expected = canonical_root(&root).unwrap();
        let actual = canonical_root(&other_root).unwrap();
        for error in [
            reopened.snapshot(&other_root, id(2)).unwrap_err(),
            reopened.restore(&other_root, &[source], &[]).unwrap_err(),
            reopened.recover(&other_root).unwrap_err(),
            session_store(&temp, OTHER_SESSION)
                .snapshot_session_start(&other_root)
                .unwrap_err(),
        ] {
            let SnapshotError::WorkspaceRootMismatch {
                expected: error_expected,
                actual: error_actual,
            } = error
            else {
                panic!("expected workspace root mismatch, got {error:?}");
            };
            assert_eq!(error_expected, expected);
            assert_eq!(error_actual, actual);
        }
        assert_eq!(read(&other_root, "file.txt"), "other");
    }

    #[cfg(unix)]
    #[test_case(SESSION_SNAPSHOTS_DIR   ; "session_snapshots")]
    #[test_case(WORKSPACE_SNAPSHOTS_DIR ; "workspace_snapshots")]
    fn a_managed_store_refuses_a_symlinked_snapshot_directory(linked: &str) {
        use std::os::unix::fs::symlink;

        let (temp, root) = setup();
        let state = StateDir::from_path(state_root(&temp));
        let outside = temp.path().join("outside");
        fs::create_dir_all(state.path()).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, state.path().join(linked)).unwrap();
        let store = SnapshotStore::new_managed(state, id(SESSION), KEY);

        assert!(store.snapshot(&root, id(1)).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn a_legacy_store_reads_as_empty_and_is_replaced_when_next_written() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        write(&store.dir.join(OBJECTS_DIR), "0123", ALPHA);
        write(&store.dir, LEGACY_MANIFEST, LEGACY_RECORD);
        write(&store.dir, JOURNAL_NAME, LEGACY_RECORD);
        write(&store.dir, UNREVERT_NAME, LEGACY_RECORD);

        assert!(store.is_legacy(), "{LEGACY_MSG}");
        assert_eq!(store.journal_state().unwrap(), None, "{LEGACY_MSG}");
        assert!(
            restore_objects(&store.dir).unwrap().is_empty(),
            "{LEGACY_MSG}"
        );

        store.snapshot_session_start(&root).unwrap();

        assert!(!store.is_legacy(), "{LEGACY_MSG}");
        for name in [LEGACY_MANIFEST, JOURNAL_NAME, UNREVERT_NAME] {
            assert!(!store.dir.join(name).exists(), "{LEGACY_MSG}");
        }
        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            ["file.txt"],
            "{LEGACY_MSG}"
        );
    }

    #[test]
    fn store_entries_report_each_workspace_with_its_sessions() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let first = session_store(&temp, SESSION);
        first.snapshot_session_start(&root).unwrap();
        first.snapshot(&root, id(1)).unwrap();
        session_store(&temp, OTHER_SESSION)
            .snapshot_session_start(&root)
            .unwrap();

        let entries = SnapshotStore::store_entries(&state_root(&temp));

        let [entry] = entries.as_slice() else {
            panic!("{LISTING_MSG}: {entries:?}");
        };
        assert_eq!(entry.workspace_key, KEY, "{LISTING_MSG}");
        assert_eq!(
            entry.root,
            Some(canonical_root(&root).unwrap()),
            "{LISTING_MSG}"
        );
        assert_eq!(entry.objects, object_count(&first), "{LISTING_MSG}");
        assert!(entry.bytes > 0, "{LISTING_MSG}");
        let listed: Vec<(&str, Vec<&str>)> = entry
            .sessions
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    session.snapshots.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        let (first_id, other_id, checkpoint) = (
            id(SESSION).to_string(),
            id(OTHER_SESSION).to_string(),
            id(1).to_string(),
        );
        assert_eq!(
            listed,
            [
                (first_id.as_str(), vec![START_POINTER, checkpoint.as_str()]),
                (other_id.as_str(), vec![START_POINTER]),
            ],
            "{LISTING_MSG}"
        );
        assert_eq!(entry.snapshot_count(), 3, "{LISTING_MSG}");
    }
}
