//! Persistent storage. `atomic_write` writes to a `tempfile` in the same
//! directory then persists (atomic rename) for crash safety.
//! `atomic_write_permissions` sets file mode before persist (for auth keys at 0600).

pub mod auth;
pub mod background;
pub mod checkout;
pub mod decision_log;
pub mod id;
pub mod input_history;
pub mod local_documents;
pub mod log;
pub mod mcp_trust;
pub mod messages;
pub mod model;
pub mod paths;
pub mod permission_config_trust;
pub mod permission_patterns;
pub mod permission_state;
pub mod plans;
pub mod private_file;
pub mod projects;
pub mod prompt_stash;
pub mod remote_operation_journal;
pub mod retention;
pub mod sandbox_auth;
pub mod sessions;
pub mod shell_durations;
pub mod shell_history;
pub mod state;
pub mod theme;
pub mod thinking;
pub mod tool_ledger;
pub mod tool_outputs;
pub mod topics;
pub mod usage_ledger;
pub mod version;
pub mod view;
pub(crate) mod words;
pub mod workbench;
#[path = "sessions/workflow.rs"]
pub mod workflow;
pub mod workflow_scratch;
pub mod workflow_source;
pub mod workflow_trust;
pub mod workspace_binding;
pub mod worktrees;

pub use words::{
    DESCRIPTIVE_ID_ATTEMPTS, DESCRIPTIVE_ID_MAX_LEN, DescriptiveIdCandidates, derived_phrase,
    random_task_id,
};

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::OnceLock;
#[cfg(windows)]
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

use paths::state_dir;

#[cfg(windows)]
const RENAME_ATTEMPTS: usize = 20;
const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
const EPHEMERAL_DIR_PREFIX: &str = "caudra";
#[cfg(unix)]
const STATE_DIRECTORY_MODE: u32 = 0o700;
const SESSION_ARTIFACT_LOCK_FILE: &str = "caudra.db.artifacts.lock";
/// How long a bounded artifact-lock wait sleeps between attempts. `flock`
/// grants no queue and no fairness, so a waiter polls rather than blocks.
const ARTIFACT_LOCK_POLL: Duration = Duration::from_millis(20);

/// Where state lives. Normally one directory. An ephemeral run splits it:
/// session data goes to a volatile root removed at exit, while credentials,
/// preferences, and trust keep using the persistent root.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
    persistent: Option<PathBuf>,
}

/// Which root a stored value belongs to during an ephemeral run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateClass {
    /// Preferences, trust, and credentials: written where a later run finds them.
    Persistent,
    /// Sessions and what they produce: discarded with an ephemeral run.
    Volatile,
}

/// Removes the volatile root when dropped. Held by the process that
/// activated ephemeral mode for as long as the run lasts.
pub struct EphemeralRoot(PathBuf);

impl EphemeralRoot {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EphemeralRoot {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.0.display(), %error, "ephemeral state root not removed");
        }
    }
}

static PROCESS_OVERRIDE: OnceLock<StateDir> = OnceLock::new();

pub struct SessionArtifactLock {
    _file: File,
}

impl StateDir {
    /// The process state directory. Returns the ephemeral split after
    /// [`Self::activate_ephemeral`] so every lazy resolver in the process
    /// agrees on where session data goes.
    pub fn resolve() -> Result<Self, StorageError> {
        if let Some(dir) = PROCESS_OVERRIDE.get() {
            return Ok(dir.clone());
        }
        Ok(Self::from_path(state_dir()?))
    }

    pub fn resolve_without_create() -> Result<Self, StorageError> {
        if let Some(dir) = PROCESS_OVERRIDE.get() {
            return Ok(dir.clone());
        }
        Ok(Self::from_path(paths::state_dir_path()?))
    }

    pub fn from_path(path: PathBuf) -> Self {
        Self {
            root: path,
            persistent: None,
        }
    }

    /// A split directory: `volatile` for sessions, `persistent` for the rest.
    pub fn split(volatile: PathBuf, persistent: PathBuf) -> Self {
        Self {
            root: volatile,
            persistent: Some(persistent),
        }
    }

    /// Creates a volatile root and makes every later [`Self::resolve`] in
    /// this process return the split. Fails when ephemeral mode was already
    /// activated.
    pub fn activate_ephemeral(persistent: Self) -> Result<(Self, EphemeralRoot), StorageError> {
        let parent = env::var_os(XDG_RUNTIME_DIR_ENV)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir())
            .unwrap_or_else(env::temp_dir);
        let (dir, root) = Self::ephemeral_in(&persistent, &parent)?;
        if PROCESS_OVERRIDE.set(dir.clone()).is_err() {
            drop(root);
            return Err(StorageError::EphemeralAlreadyActive);
        }
        Ok((dir, root))
    }

    fn ephemeral_in(
        persistent: &Self,
        parent: &Path,
    ) -> Result<(Self, EphemeralRoot), StorageError> {
        let prefix = format!("{EPHEMERAL_DIR_PREFIX}-{}-", process::id());
        let volatile = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir_in(parent)?
            .keep();
        Ok((
            Self::split(volatile.clone(), persistent.persistent_path().to_path_buf()),
            EphemeralRoot(volatile),
        ))
    }

    /// The root for session data: the volatile root during an ephemeral run.
    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn persistent_path(&self) -> &Path {
        self.persistent.as_deref().unwrap_or(&self.root)
    }

    pub fn root_for(&self, class: StateClass) -> &Path {
        match class {
            StateClass::Persistent => self.persistent_path(),
            StateClass::Volatile => self.path(),
        }
    }

    /// The directory holding values of `class`, as its own `StateDir`.
    pub fn for_class(&self, class: StateClass) -> Self {
        Self::from_path(self.root_for(class).to_path_buf())
    }

    pub fn is_ephemeral(&self) -> bool {
        self.persistent.is_some()
    }

    pub fn ensure_subdir(&self, name: impl AsRef<Path>) -> Result<PathBuf, StorageError> {
        let dir = self.root.join(name);
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(STATE_DIRECTORY_MODE);
        builder.create(&dir)?;
        Ok(dir)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("home directory not found")]
    HomeNotSet,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid provider authentication: {0}")]
    InvalidProviderAuth(String),
    #[error("invalid Workcell credential: {0}")]
    InvalidWorkcellCredential(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("slug collision after max attempts")]
    SlugCollision,
    #[error("ephemeral state was already activated for this process")]
    EphemeralAlreadyActive,
    #[error("state database: {0}")]
    Database(Box<sessions::SessionError>),
}

impl From<sessions::SessionError> for StorageError {
    fn from(error: sessions::SessionError) -> Self {
        match error {
            sessions::SessionError::Storage(error) => error,
            other => Self::Database(Box::new(other)),
        }
    }
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let tmp = staged_write(path, data)?;
    tmp.as_file().sync_data()?;
    persist(tmp, path)
}

/// Atomic against readers, but not durable until the caller runs `sync_dir`
/// on the parent. Writing a batch costs two fsyncs per file through
/// `atomic_write`, which dominates everything else once the batch is large,
/// so a batch of immutable files should share one flush instead. A lone
/// write has nothing to amortise and wants `atomic_write`.
pub fn atomic_write_deferred(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let tmp = staged_write(path, data)?;
    rename_into_place(tmp, path)
}

/// Flushes directory entries left behind by `atomic_write_deferred`.
pub fn sync_dir(dir: &Path) {
    sync_dir_handle(dir);
}

fn staged_write(path: &Path, data: &[u8]) -> Result<NamedTempFile, StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(tmp.path(), metadata.permissions())?;
    }
    Ok(tmp)
}

pub fn atomic_write_permissions(path: &Path, data: &[u8], mode: u32) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    #[cfg(unix)]
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    tmp.as_file().sync_all()?;
    persist(tmp, path)
}

pub(crate) fn exclusive_state_lock(path: &Path, mode: u32) -> Result<File, StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    validate_lock_path(path)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    options
        .mode(mode)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    #[cfg(not(unix))]
    let _ = mode;
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.lock()?;
    Ok(file)
}

pub(crate) fn try_exclusive_state_lock(
    path: &Path,
    mode: u32,
) -> Result<Option<File>, StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    validate_lock_path(path)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    options
        .mode(mode)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    #[cfg(not(unix))]
    let _ = mode;
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(crate) fn shared_state_lock(path: &Path, mode: u32) -> Result<File, StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    validate_lock_path(path)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    options
        .mode(mode)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    #[cfg(not(unix))]
    let _ = mode;
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.lock_shared()?;
    Ok(file)
}

pub(crate) fn shared_existing_state_lock(path: &Path) -> Result<File, StorageError> {
    let file = existing_state_lock(path)?;
    file.lock_shared()?;
    Ok(file)
}

pub(crate) fn try_exclusive_existing_state_lock(path: &Path) -> Result<Option<File>, StorageError> {
    let file = existing_state_lock(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn existing_state_lock(path: &Path) -> Result<File, StorageError> {
    validate_lock_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    Ok(options.open(path)?)
}

fn validate_lock_path(path: &Path) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(StorageError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("lock parent {} is not a real directory", parent.display()),
        )));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(StorageError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("lock path {} is not a regular file", path.display()),
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn lock_session_artifacts(state_dir: &StateDir) -> Result<SessionArtifactLock, StorageError> {
    let file = exclusive_state_lock(&state_dir.path().join(SESSION_ARTIFACT_LOCK_FILE), 0o600)?;
    Ok(SessionArtifactLock { _file: file })
}

/// Takes the artifact lock, giving up after `budget` instead of waiting for as
/// long as it takes.
///
/// This lock is one file for the whole state directory, so every concurrent
/// caudra contends for it whatever workspace it is in, and `flock` hands out no
/// queue: a waiter can be overtaken indefinitely by processes that keep
/// re-acquiring. Callers that must finish on a deadline take the bounded form
/// and do without the artifact rather than inherit an unbounded wait.
pub fn lock_session_artifacts_within(
    state_dir: &StateDir,
    budget: Duration,
) -> Result<Option<SessionArtifactLock>, StorageError> {
    let path = state_dir.path().join(SESSION_ARTIFACT_LOCK_FILE);
    let deadline = Instant::now() + budget;
    loop {
        if let Some(file) = try_exclusive_state_lock(&path, 0o600)? {
            return Ok(Some(SessionArtifactLock { _file: file }));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(ARTIFACT_LOCK_POLL.min(remaining));
    }
}

/// `into_parts` drops the auto-cleanup-on-drop guarantee, but we need the
/// File handle closed (Windows can't rename an open file) and tempfile's
/// `persist()` doesn't support the fibonacci backoff retry that Windows
/// virus scanners require. On failure, we manually clean up the temp file.
fn persist(tmp: NamedTempFile, path: &Path) -> Result<(), StorageError> {
    rename_into_place(tmp, path)?;
    sync_parent_dir(path);
    Ok(())
}

fn rename_into_place(tmp: NamedTempFile, path: &Path) -> Result<(), StorageError> {
    let (_, tmp_path) = tmp.into_parts();
    retry_rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StorageError::Io(e)
    })
}

pub fn durable_rename(src: &Path, dest: &Path) -> io::Result<()> {
    retry_rename(src, dest)
}

#[cfg(windows)]
pub fn durable_rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    retry_rename_with(src, dest, false)
}

/// A rename is durable only once the directory entry reaches disk; without
/// this a freshly created file can vanish after power loss even though the
/// write returned Ok. Best effort: not every filesystem accepts a directory
/// fsync. Windows gets the same from `MOVEFILE_WRITE_THROUGH` in
/// `retry_rename`.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        sync_dir_handle(dir);
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub(crate) fn sync_parent_dir_durable(path: &Path) -> Result<(), StorageError> {
    sync_parent_dir_io(path).map_err(StorageError::from)
}

pub(crate) fn sync_parent_dir_io(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_dir_handle(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Rename with fibonacci backoff to handle transient `PermissionDenied` from
/// virus scanners on Windows. 20 steps from 1ms sums to ~18 seconds.
/// Matches the pattern used by juliaup and rustup.
///
/// On non-Windows platforms, `PermissionDenied` from rename is a real
/// permissions problem (different user, immutable flag, etc.) that
/// retrying will not fix, so we just call rename once.
#[cfg(windows)]
fn retry_rename(src: &Path, dest: &Path) -> std::io::Result<()> {
    retry_rename_with(src, dest, true)
}

#[cfg(windows)]
fn retry_rename_with(src: &Path, dest: &Path, replace: bool) -> std::io::Result<()> {
    let original_permissions = if replace {
        fs::metadata(dest)
            .ok()
            .map(|metadata| metadata.permissions())
            .filter(fs::Permissions::readonly)
    } else {
        None
    };
    if let Some(permissions) = &original_permissions {
        let mut writable = permissions.clone();
        writable.set_readonly(false);
        fs::set_permissions(dest, writable)?;
    }
    let mut a: u64 = 0;
    let mut b: u64 = 1;
    let result = (|| {
        for _ in 0..RENAME_ATTEMPTS {
            match move_file_write_through(src, dest, replace) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    thread::sleep(Duration::from_millis(b));
                    let next = a.saturating_add(b);
                    a = b;
                    b = next;
                }
                Err(e) => return Err(e),
            }
        }
        move_file_write_through(src, dest, replace)
    })();
    if result.is_err()
        && let Some(permissions) = original_permissions
    {
        let _ = fs::set_permissions(dest, permissions);
    }
    result
}

#[cfg(windows)]
fn move_file_write_through(src: &Path, dest: &Path, replace: bool) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let src = windows_wide_path(src)?;
    let dest = windows_wide_path(dest)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(src.as_ptr(), dest.as_ptr(), flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let path = match path.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    };
    Ok(path.as_os_str().encode_wide().chain(Some(0)).collect())
}

#[cfg(not(windows))]
fn retry_rename(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::rename(src, dest)
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/repo";
    const INPUT_HISTORY_KEY: &str = "input.history";
    const ORIGINAL: &[u8] = b"original";
    const OWNER_ONLY_FILE_MODE: u32 = 0o600;
    const PERSISTENT_TRACE: &str = "ephemeral session data reached the persistent root";
    const PROMPT_STASH_KEY: &str = "input.stash";
    const REPLACEMENT: &[u8] = b"replacement";
    #[cfg(unix)]
    const FILE_MODE_MASK: u32 = 0o777;

    const LOCK_FREE_MSG: &str = "an uncontended artifact lock must be granted";
    const LOCK_BUSY_MSG: &str = "a held artifact lock must time out, not block forever";
    const LOCK_BUDGET_MSG: &str = "the wait must respect its budget";
    const LOCK_BUDGET: Duration = Duration::from_millis(120);

    /// The artifact lock is one file for every caudra on the machine, and
    /// `flock` offers no queue, so the bounded form is what keeps a deadline
    /// from becoming someone else's workload.
    #[test]
    fn a_held_artifact_lock_times_out_instead_of_blocking() {
        let root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(root.path().to_path_buf());

        let held = lock_session_artifacts_within(&state_dir, LOCK_BUDGET)
            .unwrap()
            .expect(LOCK_FREE_MSG);

        let started = Instant::now();
        let contended = lock_session_artifacts_within(&state_dir, LOCK_BUDGET).unwrap();
        let waited = started.elapsed();
        assert!(contended.is_none(), "{LOCK_BUSY_MSG}");
        assert!(waited >= LOCK_BUDGET, "{LOCK_BUDGET_MSG}");

        drop(held);
        assert!(
            lock_session_artifacts_within(&state_dir, LOCK_BUDGET)
                .unwrap()
                .is_some(),
            "{LOCK_FREE_MSG}"
        );
    }

    #[derive(Clone, serde::Deserialize, serde::Serialize)]
    struct TestMessage;

    impl sessions::TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    fn tree_entries(root: &Path) -> Vec<PathBuf> {
        fn collect(root: &Path, dir: &Path, entries: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                entries.push(path.strip_prefix(root).unwrap().to_path_buf());
                if path.is_dir() {
                    collect(root, &path, entries);
                }
            }
        }

        let mut entries = Vec::new();
        collect(root, root, &mut entries);
        entries.sort();
        entries
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[test]
    fn ephemeral_root_is_removed_with_its_guard() {
        let persistent_root = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(persistent_root.path().to_path_buf());
        let (state_dir, guard) = StateDir::ephemeral_in(&persistent, parent.path()).unwrap();
        let volatile = state_dir.path().to_path_buf();

        assert!(volatile.is_dir());
        assert_eq!(state_dir.persistent_path(), persistent.path());

        drop(guard);
        assert!(!volatile.exists());
    }

    #[test]
    fn ephemeral_session_data_leaves_no_persistent_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let persistent = tmp.path().join("persistent");
        let volatile = tmp.path().join("volatile");
        fs::create_dir(&persistent).unwrap();
        let persistent_dir = StateDir::from_path(persistent.clone());
        let database = sessions::SessionDatabase::open_state(&persistent_dir).unwrap();
        assert!(database.session_facts(None).unwrap().is_empty());
        drop(database);
        let before = tree_entries(&persistent);
        let state_dir = StateDir::split(volatile.clone(), persistent.clone());

        let mut session = sessions::Session::<TestMessage, (), serde_json::Value>::new("test", CWD);
        let session_id = session.id;
        session.save(&state_dir).unwrap();
        tool_outputs::ToolOutputStore::new(state_dir.clone())
            .put(session_id, "tool output")
            .unwrap();
        let mut history = input_history::InputHistory::load(&state_dir, 10);
        history.push("prompt".into());
        prompt_stash::PromptStash::open(&state_dir)
            .unwrap()
            .push(prompt_stash::StashDraft {
                text: "draft".into(),
                cwd: CWD.into(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(tree_entries(&persistent), before, "{PERSISTENT_TRACE}");
        let database = sessions::SessionDatabase::open_state(&persistent_dir).unwrap();
        assert!(
            database.session_facts(None).unwrap().is_empty(),
            "{PERSISTENT_TRACE}"
        );
        assert!(
            database
                .state_get::<serde_json::Value>(state::SCOPE_GLOBAL, INPUT_HISTORY_KEY)
                .unwrap()
                .is_none(),
            "{PERSISTENT_TRACE}"
        );
        assert!(
            database
                .state_get::<serde_json::Value>(state::SCOPE_GLOBAL, PROMPT_STASH_KEY)
                .unwrap()
                .is_none(),
            "{PERSISTENT_TRACE}"
        );
        assert!(volatile.join(sessions::SESSIONS_DB_FILE).is_file());
        assert!(volatile.join(tool_outputs::TOOL_OUTPUT_DIR).is_dir());
    }

    #[test]
    fn atomic_write_permissions_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write_permissions(&path, REPLACEMENT, OWNER_ONLY_FILE_MODE).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");

        atomic_write(&path, ORIGINAL).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            OWNER_ONLY_FILE_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_destination_permissions() {
        const MODE: u32 = 0o640;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(MODE)).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_cleans_up_temp_after_replacement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write(&destination, REPLACEMENT).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
