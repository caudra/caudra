use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use etcetera::base_strategy::BaseStrategy;

const fn directory_names(debug_assertions: bool) -> (&'static str, &'static str) {
    if debug_assertions {
        ("caudra-debug", ".caudra-debug")
    } else {
        ("caudra", ".caudra")
    }
}

const DIRECTORY_NAMES: (&str, &str) = directory_names(cfg!(debug_assertions));
const APP_DIR_NAME: &str = DIRECTORY_NAMES.0;
const LEGACY_DIR_NAME: &str = DIRECTORY_NAMES.1;
pub const XDG_MIGRATED_MARKER: &str = ".xdg-migrated";

static STRATEGY: OnceLock<Option<Paths>> = OnceLock::new();

struct Paths {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
    xdg_config: PathBuf,
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

fn paths_for(
    strategy: &impl BaseStrategy,
    fallback_dir: Option<&Path>,
    app_dir_name: &str,
) -> Paths {
    let xdg_config = strategy.config_dir().join(app_dir_name);
    let (data, cache, config) = match fallback_dir {
        Some(dir) => (dir.to_path_buf(), dir.to_path_buf(), dir.to_path_buf()),
        None => (
            strategy.data_dir().join(app_dir_name),
            strategy.cache_dir().join(app_dir_name),
            xdg_config.clone(),
        ),
    };
    let (state, logs) = if fallback_dir.is_some() {
        (data.clone(), data.clone())
    } else {
        state_logs(strategy, &data, app_dir_name)
    };
    Paths {
        config,
        data,
        state,
        logs,
        cache,
        xdg_config,
    }
}

fn existing_legacy_dir(home: Option<PathBuf>, legacy_dir_name: &str) -> Option<PathBuf> {
    home.map(|path| path.join(legacy_dir_name))
        // XDG cutover leaves a durable marker so cached legacy paths cannot
        // recreate state while new processes select XDG.
        .filter(|path| path.is_dir() && !path.join(XDG_MIGRATED_MARKER).is_file())
}

fn resolve() -> Option<&'static Paths> {
    STRATEGY
        .get_or_init(|| {
            let s = etcetera::choose_base_strategy().ok()?;
            let fallback_dir = existing_legacy_dir(etcetera::home_dir().ok(), LEGACY_DIR_NAME);
            Some(paths_for(&s, fallback_dir.as_deref(), APP_DIR_NAME))
        })
        .as_ref()
}

fn err() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot determine base directories",
    )
}

fn ensure(path: &Path) -> Result<PathBuf, std::io::Error> {
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

fn xdg_only_paths() -> Result<Paths, std::io::Error> {
    let strategy = etcetera::choose_base_strategy().map_err(|_| err())?;
    Ok(paths_for(&strategy, None, APP_DIR_NAME))
}

fn active_path(field: fn(&Paths) -> &Path) -> Result<PathBuf, std::io::Error> {
    let cached = resolve().ok_or_else(err)?;
    if cached.state.join(XDG_MIGRATED_MARKER).is_file() {
        let xdg = xdg_only_paths()?;
        return ensure(field(&xdg));
    }
    ensure(field(cached))
}

pub fn config_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.config)
}

pub fn xdg_config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.xdg_config)
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.data)
}

pub fn state_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.state)
}

pub fn logs_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.logs)
}

pub fn cache_dir() -> Result<PathBuf, std::io::Error> {
    active_path(|paths| &paths.cache)
}

pub struct XdgPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
}

pub fn xdg_paths() -> Result<XdgPaths, std::io::Error> {
    let s = etcetera::choose_base_strategy().map_err(|_| err())?;
    let paths = paths_for(&s, None, APP_DIR_NAME);
    Ok(XdgPaths {
        config: paths.config,
        state: paths.state,
        logs: paths.logs,
    })
}

pub fn home() -> Option<PathBuf> {
    etcetera::home_dir().ok()
}

pub fn legacy_home_dir() -> Option<PathBuf> {
    existing_legacy_dir(etcetera::home_dir().ok(), LEGACY_DIR_NAME)
}

/// Candidate config directories for `subdir` from `home` and `xdg_config`.
/// Pure: no env reads, no process-home fallback. Production callers pass
/// `config_dir().ok()` as `xdg_config` (which honors `XDG_CONFIG_HOME`, the
/// active legacy fallback, and the Windows `AppData\Roaming` strategy via
/// `resolve()`); tests pass tempdirs.
pub fn user_config_dirs(
    home: Option<&Path>,
    xdg_config: Option<&Path>,
    subdir: &str,
) -> Vec<PathBuf> {
    let legacy = home.map(|h| h.join(LEGACY_DIR_NAME).join(subdir));
    let xdg = xdg_config.map(|d| d.join(subdir));
    [legacy, xdg].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn directory_names_are_isolated_by_build_profile() {
        assert_eq!(directory_names(false), ("caudra", ".caudra"));
        assert_eq!(directory_names(true), ("caudra-debug", ".caudra-debug"));
    }

    #[test]
    fn paths_use_selected_app_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), true);

        let paths = paths_for(&strategy, None, "caudra-debug");

        assert_eq!(paths.config, root.path().join("config/caudra-debug"));
        assert_eq!(paths.data, root.path().join("data/caudra-debug"));
        assert_eq!(paths.state, root.path().join("state/caudra-debug"));
        assert_eq!(paths.logs, root.path().join("logs/caudra-debug"));
        assert_eq!(paths.cache, root.path().join("cache/caudra-debug"));
        assert_eq!(paths.xdg_config, root.path().join("config/caudra-debug"));
    }

    #[test]
    fn paths_without_state_directory_use_namespaced_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), false);

        let paths = paths_for(&strategy, None, "caudra-debug");
        let data = root.path().join("data/caudra-debug");

        assert_eq!(paths.state, data);
        assert_eq!(paths.logs, data);
    }

    #[test]
    fn debug_legacy_directory_does_not_select_release_directory() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".caudra")).unwrap();

        assert_eq!(
            existing_legacy_dir(Some(home.path().into()), ".caudra-debug"),
            None
        );

        let debug = home.path().join(".caudra-debug");
        fs::create_dir(&debug).unwrap();
        assert_eq!(
            existing_legacy_dir(Some(home.path().into()), ".caudra-debug"),
            Some(debug)
        );
    }

    #[test]
    fn legacy_directory_collapses_all_active_paths() {
        let root = tempfile::tempdir().unwrap();
        let strategy = TestStrategy::new(root.path(), true);
        let legacy = root.path().join("home/.caudra-debug");

        let paths = paths_for(&strategy, Some(&legacy), "caudra-debug");

        assert_eq!(paths.config, legacy);
        assert_eq!(paths.data, legacy);
        assert_eq!(paths.state, legacy);
        assert_eq!(paths.logs, legacy);
        assert_eq!(paths.cache, legacy);
        assert_eq!(paths.xdg_config, root.path().join("config/caudra-debug"));
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
    fn user_config_dirs_returns_legacy_and_xdg() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join(".config").join(APP_DIR_NAME);

        let dirs = user_config_dirs(Some(home.path()), Some(&xdg), "AGENTS.md");
        assert_eq!(
            dirs,
            vec![
                home.path().join(LEGACY_DIR_NAME).join("AGENTS.md"),
                xdg.join("AGENTS.md"),
            ]
        );
    }

    #[test]
    fn user_config_dirs_omits_legacy_when_home_none() {
        let xdg = tempfile::tempdir().unwrap();

        let dirs = user_config_dirs(None, Some(xdg.path()), "AGENTS.md");
        assert_eq!(dirs, vec![xdg.path().join("AGENTS.md")]);
    }

    #[test]
    fn user_config_dirs_omits_xdg_when_xdg_none() {
        let home = tempfile::tempdir().unwrap();

        let dirs = user_config_dirs(Some(home.path()), None, "AGENTS.md");
        assert_eq!(
            dirs,
            vec![home.path().join(LEGACY_DIR_NAME).join("AGENTS.md")]
        );
    }

    #[test]
    fn user_config_dirs_neither_depends_on_process_env() {
        let home_a = tempfile::tempdir().unwrap();
        let xdg_a = home_a.path().join(".config").join(APP_DIR_NAME);

        let hostile = tempfile::tempdir().unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: tests run single-threaded within a process nextest invokes once.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", hostile.path()) };

        let dirs = user_config_dirs(Some(home_a.path()), Some(&xdg_a), "AGENTS.md");

        // SAFETY: same single-threaded assumption as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            !dirs.iter().any(|p| p.starts_with(hostile.path())),
            "combiner read XDG_CONFIG_HOME: {dirs:?}"
        );
    }
}
