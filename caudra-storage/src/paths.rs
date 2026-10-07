use std::env;
use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use etcetera::base_strategy::BaseStrategy;

const NAMESPACE_ENV: &str = "CAUDRA_NAMESPACE";
#[cfg(unix)]
const DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const GROUP_OTHER_WRITE: u32 = 0o022;

/// Debug builds get their own directory so a development run never shares
/// config, sessions, auth, logs, or caches with an installed release.
const fn app_dir_name(debug_assertions: bool) -> &'static str {
    if debug_assertions {
        "caudra-debug"
    } else {
        "caudra"
    }
}

const APP_DIR_NAME: &str = app_dir_name(cfg!(debug_assertions));

/// Rejection of an explicit namespace. The override is authoritative: an
/// unusable value refuses to start rather than quietly falling back to the
/// build-profile default and writing to a directory nobody asked for.
#[derive(Debug, Clone, thiserror::Error)]
pub enum NamespaceError {
    #[error("{NAMESPACE_ENV} is not valid UTF-8")]
    NotUtf8,
    #[error("{NAMESPACE_ENV} must be a single directory name, got `{0}`")]
    NotASegment(String),
}

#[derive(Debug, Clone, thiserror::Error)]
enum PathsError {
    #[error("cannot determine base directories")]
    BaseDirs,
    #[error(transparent)]
    Namespace(#[from] NamespaceError),
}

static STRATEGY: OnceLock<Result<Paths, PathsError>> = OnceLock::new();

struct Paths {
    app_dir_name: String,
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
    scratch: PathBuf,
}

/// Parse an explicit namespace override. Pure: the caller supplies the raw
/// value, so nothing here reads process environment.
fn namespace_from(raw: Option<&OsStr>) -> Result<Option<&str>, NamespaceError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = raw.to_str().ok_or(NamespaceError::NotUtf8)?.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(segment)), None) if segment.to_str() == Some(value) => {
            Ok(Some(value))
        }
        _ => Err(NamespaceError::NotASegment(value.to_owned())),
    }
}

/// Lexical path normalization that never hits the filesystem.
///
/// Returns an absolute path with `..` and `.` components resolved, but without
/// calling `canonicalize`. This means no `\\?\` prefix on Windows and no symlink
/// resolution. Use this for display, logging, and scope matching.
pub fn normalize_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    normalize_abs_path(&abs)
}

fn normalize_abs_path(abs: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::ParentDir => {
                // Only pop if the trailing component is a normal directory,
                // never a root or prefix.
                if let Some(Component::Normal(_)) = result.components().next_back() {
                    result.pop();
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Canonicalize a path (resolving symlinks) but strip the `\\?\` prefix
/// that Windows adds. Falls back to `normalize_path` if the path does not
/// exist yet.
///
/// Contract: the input is a "normal" path (no `\\?\` prefix). The output is
/// always display-friendly: no `\\?\`, no `..` components. On Windows UNC
/// paths (`\\?\UNC\server\share`), the result is `\\server\share`.
///
/// The result is for display, logging, and scope matching only. Do not pass
/// it to Win32 filesystem APIs if the path exceeds 260 characters (the
/// `\\?\` prefix is what bypasses that limit).
pub fn canonicalize_clean(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(canon) => strip_windows_extended_prefix(&canon),
        Err(_) => normalize_path(path),
    }
}

/// Canonicalize a path by resolving each component left-to-right through
/// the filesystem.
///
/// At each step, the accumulated path is canonicalized so that symlinks
/// are resolved *before* a subsequent `..` component can traverse through
/// them. For non-existent tail components, falls back to lexical append.
///
/// This is the correct canonicalization for security-sensitive path checks
/// (boundary verification, scope matching) where symlink escapes matter.
/// Unlike `canonicalize_clean`, this never resolves `..` lexically when
/// a symlink is in play.
///
/// Returns `None` if the root/prefix portion of the path cannot be resolved.
pub fn incremental_canonicalize(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let next = current.join("..");
                if let Ok(canon) = next.canonicalize() {
                    current = strip_windows_extended_prefix(&canon);
                } else if let Some(Component::Normal(_)) = current.components().next_back() {
                    current.pop();
                }
            }
            Component::Normal(name) => {
                let next = current.join(name);
                match next.canonicalize() {
                    Ok(canon) => current = strip_windows_extended_prefix(&canon),
                    Err(_) => {
                        // `current` is already canonical from a prior iteration,
                        // so we can append the non-existent tail directly without
                        // re-resolving the parent.
                        current = next;
                    }
                }
            }
        }
    }

    if current.as_os_str().is_empty() {
        None
    } else {
        Some(current)
    }
}

/// Strip the `\\?\` prefix that Windows `canonicalize` adds, using the
/// Rust `Prefix` enum for correct WTF-8 handling (no `.to_str()` lossy
/// conversion).
///
/// `\\?\C:\foo` becomes `C:\foo`.
/// `\\?\UNC\server\share\dir` becomes `\\server\share\dir`.
///
/// **Contract**: the result is for display, logging, and scope matching only.
/// Do not pass it to Win32 filesystem APIs if the path exceeds 260 characters
/// (the `\\?\` prefix is what bypasses that limit).
#[cfg(windows)]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    use std::path::Prefix;

    let mut components = canon.components();
    let Some(Component::Prefix(pfx)) = components.next() else {
        return canon.to_path_buf();
    };
    let rest = components.as_path();
    match pfx.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)).join(rest),
        Prefix::VerbatimUNC(server, share) => {
            let mut base = PathBuf::from(r"\\");
            base.push(server);
            base.push(share);
            base.join(rest)
        }
        _ => canon.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    canon.to_path_buf()
}

fn state_logs(s: &impl BaseStrategy, fallback: &Path, app_dir_name: &str) -> (PathBuf, PathBuf) {
    let state_base = s.state_dir();
    let state = state_base
        .as_ref()
        .map(|d| d.join(app_dir_name))
        .unwrap_or_else(|| fallback.to_path_buf());
    let logs = state_base
        .as_ref()
        .and_then(|d| d.parent().map(|p| p.join("logs").join(app_dir_name)))
        .unwrap_or_else(|| fallback.to_path_buf());
    (state, logs)
}

/// `temp_root` is a parameter rather than an `env::temp_dir()` read so the join
/// stays pure and testable, and so the caller controls when it is sampled:
/// [`scratch_dir`] is what `TMPDIR` is later pointed at, and reading the
/// variable after that redirect would nest the namespace inside itself.
fn paths_for(strategy: &impl BaseStrategy, app_dir_name: &str, temp_root: &Path) -> Paths {
    let config = strategy.config_dir().join(app_dir_name);
    let data = strategy.data_dir().join(app_dir_name);
    let cache = strategy.cache_dir().join(app_dir_name);
    let scratch = temp_root.join(app_dir_name);
    let (state, logs) = state_logs(strategy, &data, app_dir_name);
    Paths {
        app_dir_name: app_dir_name.to_owned(),
        config,
        data,
        state,
        logs,
        cache,
        scratch,
    }
}

fn resolve() -> Result<&'static Paths, &'static PathsError> {
    STRATEGY
        .get_or_init(|| {
            let raw = env::var_os(NAMESPACE_ENV);
            let namespace = namespace_from(raw.as_deref())?;
            let strategy = etcetera::choose_base_strategy().map_err(|_| PathsError::BaseDirs)?;
            Ok(paths_for(
                &strategy,
                namespace.unwrap_or(APP_DIR_NAME),
                &env::temp_dir(),
            ))
        })
        .as_ref()
}

fn err(error: &PathsError) -> std::io::Error {
    let kind = match error {
        PathsError::BaseDirs => std::io::ErrorKind::NotFound,
        PathsError::Namespace(_) => std::io::ErrorKind::InvalidInput,
    };
    std::io::Error::new(kind, error.to_string())
}

/// Validate an explicit namespace once, before anything touches a platform
/// directory, so a rejected value is reported where the user can act on it.
pub fn check_namespace_override() -> Result<(), NamespaceError> {
    match resolve() {
        Err(PathsError::Namespace(error)) => Err(error.clone()),
        _ => Ok(()),
    }
}

/// Parents belong to the platform and keep whatever mode it gives them; the
/// directory Caudra owns is created owner-only so that what lands on disk does
/// not depend on the umask of whichever run happened to create it first.
/// [`PrivateFile`](crate::private_file::PrivateFile) refuses to read through a
/// group-writable ancestor, so a permissive umask here makes config, auth, and
/// sandbox state unreadable.
fn ensure(path: &Path) -> Result<PathBuf, std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_private_dir(path)
}

/// Create one owner-only directory, refusing to accept an entry somebody else
/// put there first.
///
/// The scratch tree is the one part of Caudra that lives in a world-writable
/// directory under a predictable name, `TMPDIR` aims every child process at it,
/// and permission policy pre-allows writes below it. A local user who wins the
/// race and leaves a symlink behind would redirect all three. `create_dir_all`
/// follows such a link without complaint, so every directory Caudra owns is
/// created one at a time through this instead.
pub fn ensure_private_dir(path: &Path) -> Result<PathBuf, std::io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let builder = &mut fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(DIRECTORY_MODE);
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to use {} as a private directory", path.display()),
        ));
    }
    Ok(path.to_path_buf())
}

/// Repair the directories Caudra created for itself before it guaranteed their
/// mode, returning the ones actually changed so startup can say so.
///
/// Only Caudra's own directories are touched. An ancestor such as `~/.config`
/// is shared with every other application and its mode is not ours to decide,
/// and a directory owned by somebody else is a refusal to report rather than a
/// permission to change. Duplicates need no filtering: a path repaired once is
/// already owner-only when it is reached again as another role.
pub fn tighten_private_dirs() -> Vec<PathBuf> {
    let Ok(paths) = resolve() else {
        return Vec::new();
    };
    [
        &paths.config,
        &paths.data,
        &paths.state,
        &paths.logs,
        &paths.cache,
    ]
    .into_iter()
    .filter(|path| tighten_private_dir(path))
    .cloned()
    .collect()
}

#[cfg(unix)]
fn tighten_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let mode = metadata.mode() & 0o777;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || mode & GROUP_OTHER_WRITE == 0
    {
        return false;
    }
    match fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE)) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                previous_mode = format!("{mode:o}"),
                mode = format!("{DIRECTORY_MODE:o}"),
                "tightened a group-writable Caudra directory to owner-only"
            );
            true
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                previous_mode = format!("{mode:o}"),
                %error,
                "cannot tighten a group-writable Caudra directory; private files under it stay unreadable"
            );
            false
        }
    }
}

#[cfg(not(unix))]
fn tighten_private_dir(_path: &Path) -> bool {
    false
}

fn active_path(field: fn(&Paths) -> &Path) -> Result<PathBuf, std::io::Error> {
    ensure(field(resolve().map_err(err)?))
}

pub fn config_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.config)
}

pub fn config_dir_path() -> Result<PathBuf, std::io::Error> {
    Ok(resolve().map_err(err)?.config.clone())
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.data)
}

pub fn state_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.state)
}

pub fn state_dir_path() -> Result<PathBuf, std::io::Error> {
    Ok(resolve().map_err(err)?.state.clone())
}

pub fn logs_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.logs)
}

pub fn logs_dir_path() -> Result<PathBuf, std::io::Error> {
    Ok(resolve().map_err(err)?.logs.clone())
}

pub fn cache_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.cache)
}

pub fn cache_dir_path() -> Result<PathBuf, std::io::Error> {
    Ok(resolve().map_err(err)?.cache.clone())
}

/// The root every project's scratch directory sits under. Namespaced like every
/// other directory, so a debug build never shares scratch with a release.
///
/// Permission policy pre-allows the whole root rather than the current project's
/// subdirectory. `/cd` rebinds the project but cannot move `TMPDIR`, which is
/// fixed for the life of the process, so a narrower grant would strand a path
/// the model was already handed.
///
/// Resolving this fixes the temp root for the process, which is why the redirect
/// that points `TMPDIR` inside it must call this before setting the variable.
pub fn scratch_root() -> Result<PathBuf, std::io::Error> {
    ensure_private_dir(&resolve().map_err(err)?.scratch)
}

/// The single directory name every platform root is namespaced by, including
/// the `CAUDRA_NAMESPACE` override. A remote scratch directory joins it to a
/// temp root this process cannot see, so the name has to be available apart
/// from any local path built from it.
pub fn active_app_dir_name() -> Result<&'static str, std::io::Error> {
    Ok(&resolve().map_err(err)?.app_dir_name)
}

pub fn home() -> Option<PathBuf> {
    etcetera::home_dir().ok()
}

/// The user-level config directory for `subdir`. Pure: no env reads, no
/// process-home fallback. Production callers pass `config_dir().ok()`, which
/// honors `XDG_CONFIG_HOME` and the Windows `AppData\Roaming` strategy via
/// `resolve()`; tests pass tempdirs.
pub fn user_config_dir(config: Option<&Path>, subdir: &str) -> Option<PathBuf> {
    config.map(|dir| dir.join(subdir))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;
    #[cfg(unix)]
    const SHARED_MODE: u32 = 0o755;
    #[cfg(unix)]
    const GROUP_WRITABLE_MODE: u32 = 0o775;

    struct TestStrategy {
        home: PathBuf,
        config: PathBuf,
        data: PathBuf,
        cache: PathBuf,
        state: Option<PathBuf>,
    }

    impl TestStrategy {
        fn new(root: &Path, has_state_dir: bool) -> Self {
            Self {
                home: root.join("home"),
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
                state: has_state_dir.then(|| root.join("state")),
            }
        }
    }

    impl BaseStrategy for TestStrategy {
        fn home_dir(&self) -> &Path {
            &self.home
        }

        fn config_dir(&self) -> PathBuf {
            self.config.clone()
        }

        fn data_dir(&self) -> PathBuf {
            self.data.clone()
        }

        fn cache_dir(&self) -> PathBuf {
            self.cache.clone()
        }

        fn state_dir(&self) -> Option<PathBuf> {
            self.state.clone()
        }

        fn runtime_dir(&self) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn app_directory_name_is_isolated_by_build_profile() {
        assert_eq!(app_dir_name(false), "caudra");
        assert_eq!(app_dir_name(true), "caudra-debug");
    }

    #[test_case("caudra-debug" ; "build profile name")]
    #[test_case("caudra-scratch" ; "overridden name")]
    fn paths_use_selected_app_directory_name(name: &str) {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), true);

        let paths = paths_for(&strategy, name, &root.path().join("tmp"));

        assert_eq!(paths.config, root.path().join("config").join(name));
        assert_eq!(paths.data, root.path().join("data").join(name));
        assert_eq!(paths.state, root.path().join("state").join(name));
        assert_eq!(paths.logs, root.path().join("logs").join(name));
        assert_eq!(paths.cache, root.path().join("cache").join(name));
        assert_eq!(paths.scratch, root.path().join("tmp").join(name));
    }

    /// The whole reason the scratch levels bypass `create_dir_all`: another
    /// local user can win the race in a world-writable temp root, and a
    /// symlink left behind would silently redirect everything `TMPDIR` aims
    /// at the directory, including writes permission policy pre-allows.
    #[test]
    #[cfg(unix)]
    fn a_planted_symlink_is_refused_rather_than_followed() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        let planted = root.path().join("planted");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &planted).unwrap();

        let error = ensure_private_dir(&planted).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    #[cfg(unix)]
    fn a_created_directory_is_owner_only_and_reopening_it_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("scratch");

        let created = ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&created).unwrap().permissions().mode();

        assert_eq!(mode & MODE_MASK, DIRECTORY_MODE);
        assert_eq!(ensure_private_dir(&dir).unwrap(), created);
    }

    /// The parent is somebody else's directory — `~/.config` is shared with
    /// every other application — so creating ours must not restyle it, while
    /// ours lands owner-only whatever umask the run happened to have.
    #[test]
    #[cfg(unix)]
    fn ensure_creates_the_parent_without_deciding_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("config");
        let ours = parent.join(APP_DIR_NAME);
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(SHARED_MODE)).unwrap();

        let created = ensure(&ours).unwrap();

        assert_eq!(mode_of(&created), DIRECTORY_MODE);
        assert_eq!(mode_of(&parent), SHARED_MODE);
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & MODE_MASK
    }

    #[cfg(unix)]
    #[test_case(0o775 ; "group writable")]
    #[test_case(0o777 ; "world writable")]
    #[test_case(0o702 ; "other write only")]
    fn a_writable_directory_is_repaired_without_touching_its_parent(mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("config");
        let ours = parent.join(APP_DIR_NAME);
        fs::create_dir_all(&ours).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(SHARED_MODE)).unwrap();
        fs::set_permissions(&ours, fs::Permissions::from_mode(mode)).unwrap();

        assert!(tighten_private_dir(&ours));

        assert_eq!(mode_of(&ours), DIRECTORY_MODE);
        assert_eq!(mode_of(&parent), SHARED_MODE);
    }

    /// Only write is rejected, so a group-readable directory is sound and stays
    /// as the user left it. Tightening it would be a preference, not a repair.
    #[cfg(unix)]
    #[test_case(DIRECTORY_MODE ; "already owner only")]
    #[test_case(0o750 ; "group readable but not writable")]
    fn a_sound_directory_is_left_as_the_user_left_it(mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(APP_DIR_NAME);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();

        assert!(!tighten_private_dir(&dir));
        assert_eq!(mode_of(&dir), mode);
    }

    /// A symlink is [`ensure_private_dir`]'s refusal to report rather than a
    /// mode to repair, and chasing it would tighten a directory Caudra does not
    /// own. An absent directory is created owner-only, never chmodded.
    #[test]
    #[cfg(unix)]
    fn a_planted_symlink_and_an_absent_directory_are_not_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        let planted = root.path().join("planted");
        fs::create_dir(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(GROUP_WRITABLE_MODE)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &planted).unwrap();

        assert!(!tighten_private_dir(&planted));
        assert!(!tighten_private_dir(&root.path().join("absent")));
        assert_eq!(mode_of(&elsewhere), GROUP_WRITABLE_MODE);
    }

    #[test]
    fn scratch_sits_beside_the_temp_root_rather_than_inside_a_previous_scratch() {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), true);
        let temp_root = root.path().join("tmp");

        let first = paths_for(&strategy, APP_DIR_NAME, &temp_root);
        let second = paths_for(&strategy, APP_DIR_NAME, &temp_root);

        assert_eq!(first.scratch, second.scratch);
        assert_eq!(first.scratch.parent(), Some(temp_root.as_path()));
    }

    #[test_case(None, None ; "unset")]
    #[test_case(Some(""), None ; "empty")]
    #[test_case(Some("   "), None ; "whitespace only")]
    #[test_case(Some("caudra"), Some("caudra") ; "release name")]
    #[test_case(Some("caudra-scratch"), Some("caudra-scratch") ; "custom name")]
    #[test_case(Some(" caudra-scratch "), Some("caudra-scratch") ; "surrounding whitespace")]
    #[test_case(Some("work.space"), Some("work.space") ; "interior dot")]
    fn namespace_override_accepts_a_single_segment(raw: Option<&str>, expected: Option<&str>) {
        assert_eq!(namespace_from(raw.map(OsStr::new)).unwrap(), expected);
    }

    #[test_case("." ; "current directory")]
    #[test_case(".." ; "parent directory")]
    #[test_case("a/b" ; "nested")]
    #[test_case("a/" ; "trailing separator")]
    #[test_case("/abs" ; "absolute")]
    #[test_case("../escape" ; "traversal")]
    #[cfg_attr(windows, test_case(r"C:\x" ; "windows drive"))]
    fn namespace_override_rejects_anything_that_is_not_a_segment(raw: &str) {
        assert!(matches!(
            namespace_from(Some(OsStr::new(raw))),
            Err(NamespaceError::NotASegment(_))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn namespace_override_rejects_non_utf8() {
        use std::os::unix::ffi::OsStrExt;

        assert!(matches!(
            namespace_from(Some(OsStr::from_bytes(&[0xff]))),
            Err(NamespaceError::NotUtf8)
        ));
    }

    #[test]
    fn paths_without_state_directory_use_namespaced_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), false);

        let paths = paths_for(&strategy, "caudra-debug", &root.path().join("tmp"));
        let data = root.path().join("data/caudra-debug");

        assert_eq!(paths.state, data);
        assert_eq!(paths.logs, data);
    }

    #[test]
    fn normalize_path_resolves_parent() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join("b").join("..").join("c");
        let expected = cwd.join("a").join("c");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_resolves_dot() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join(".").join("b");
        let expected = cwd.join("a").join("b");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_does_not_pop_past_root() {
        // /../etc should produce /etc, not the relative "etc"
        let result = normalize_path(Path::new("/../etc"));
        assert!(result.is_absolute(), "must stay absolute: {result:?}");
        #[cfg(unix)]
        assert_eq!(result, PathBuf::from("/etc"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_local_drive() {
        let input = Path::new(r"\\?\C:\Users\test\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\Users\test\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_unc_share() {
        let input = Path::new(r"\\?\UNC\server\share\dir\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"\\server\share\dir\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_no_prefix() {
        let input = Path::new(r"C:\already\normal\path.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\already\normal\path.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn canonicalize_clean_strips_extended_prefix() {
        let tmp = std::env::temp_dir();
        let result = canonicalize_clean(&tmp);
        let s = result.to_str().unwrap();
        assert!(
            !s.starts_with(r"\\?\"),
            "should not have \\\\?\\ prefix: {s}"
        );
    }

    #[test]
    fn user_config_dir_joins_the_given_config_root() {
        let config = tempfile::tempdir().unwrap();

        assert_eq!(
            user_config_dir(Some(config.path()), "AGENTS.md"),
            Some(config.path().join("AGENTS.md"))
        );
        assert_eq!(user_config_dir(None, "AGENTS.md"), None);
    }

    #[test]
    fn user_config_dir_does_not_depend_on_process_env() {
        let config = tempfile::tempdir().unwrap();
        let hostile = tempfile::tempdir().unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: tests run single-threaded within a process nextest invokes once.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", hostile.path()) };

        let dir = user_config_dir(Some(config.path()), "AGENTS.md");

        // SAFETY: same single-threaded assumption as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert_eq!(dir, Some(config.path().join("AGENTS.md")));
    }
}
