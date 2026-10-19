//! Watches every place Caudra reads configuration from and reports which
//! [`ConfigSource`]s changed, so each one reloads on its own.
//!
//! Paths are matched against where the loaders actually read: a project's
//! `.caudra/caudra.toml` only counts in the working directory, while skills
//! and commands count in every project ancestor up to the repository root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use caudra_config::PROJECT_DIR;
use caudra_config::config_file::{CONFIG_FILE, INIT_LUA_FILE};
use caudra_config::files::{ENV, PERMISSIONS, PROVIDERS, WORKCELL};
use caudra_config::mcp::MCP_FILE;
use caudra_config::sandbox::SANDBOX_FILE;
use caudra_storage::paths;
use flume::{Receiver, RecvTimeoutError};
use notify::event::ModifyKind;
use notify::{
    Config, Error as WatchError, ErrorKind, Event, EventKind, EventKindMask, RecommendedWatcher,
    RecursiveMode, Result as Watched, Watcher as _,
};
use tracing::{debug, warn};

use crate::automation::catalog::AUTOMATIONS_SUBDIR;
use crate::command::{
    COMMANDS_SUBDIR, GLOBAL_THIRD_PARTY_COMMAND_DIRS, PROJECT_COMMAND_DIRS,
    find_project_ancestor_dirs,
};
use crate::decisions::PERMISSION_QUESTION_FILE;
use crate::prompt::profile::PROFILE_DIR;
use crate::tools::native::skill::{GLOBAL_SKILL_DIRS, PROJECT_SKILL_DIRS, SKILLS_SUBDIR};
use crate::workflow::catalog::WORKFLOWS_SUBDIR;

const THREAD_NAME: &str = "caudra-config-watch";
/// How long the sources have to stay quiet before a change is reported.
const QUIET: Duration = Duration::from_millis(250);
/// The longest a change is held back, so a file rewritten continuously still
/// reloads.
const MAX_DELAY: Duration = Duration::from_secs(2);
/// Where `require` finds the modules of the `init.lua` beside it; caudra-lua's
/// runtime joins it onto the plugin directory.
const LUA_MODULES_DIR: &str = "lua";
const TEMP_SUFFIXES: &[&str] = &["~", ".swp", ".swo", ".swx"];
const TEMP_PREFIX: &str = ".#";
/// Vim writes and removes this beside a file to learn whether it may write
/// the directory.
const VIM_WRITE_PROBE: &str = "4913";

/// Under the global config directory. Its `.env` feeds every global file that
/// expands `{env:NAME}` through `Lookup::Global`; sandboxes.toml is literal.
const GLOBAL_RULES: &[Rule] = &[
    (CONFIG_FILE, &[ConfigSource::Settings]),
    (PERMISSIONS.name, &[ConfigSource::Permissions]),
    (MCP_FILE, &[ConfigSource::Mcp]),
    (PROVIDERS.name, &[ConfigSource::Providers]),
    (PERMISSION_QUESTION_FILE, &[ConfigSource::DecisionQuestions]),
    (WORKCELL.name, &[ConfigSource::Workcell]),
    (SANDBOX_FILE, &[ConfigSource::Sandboxes]),
    (INIT_LUA_FILE, &[ConfigSource::InitLua]),
    (LUA_MODULES_DIR, &[ConfigSource::InitLua]),
    (
        ENV.name,
        &[
            ConfigSource::GlobalEnv,
            ConfigSource::Settings,
            ConfigSource::Permissions,
            ConfigSource::Mcp,
            ConfigSource::Providers,
            ConfigSource::Workcell,
        ],
    ),
    (SKILLS_SUBDIR, &[ConfigSource::Skills]),
    (PROFILE_DIR, &[ConfigSource::PromptProfiles]),
    (COMMANDS_SUBDIR, &[ConfigSource::Commands]),
    (WORKFLOWS_SUBDIR, &[ConfigSource::Workflows]),
    (AUTOMATIONS_SUBDIR, &[ConfigSource::Automations]),
];
/// Under the working directory's `.caudra`, the only project directory these
/// are read from. Project files refuse `{env:}`, so its `.env` feeds nothing
/// else.
const PROJECT_RULES: &[Rule] = &[
    (CONFIG_FILE, &[ConfigSource::Settings]),
    (PERMISSIONS.name, &[ConfigSource::Permissions]),
    (MCP_FILE, &[ConfigSource::Mcp]),
    (INIT_LUA_FILE, &[ConfigSource::InitLua]),
    (LUA_MODULES_DIR, &[ConfigSource::InitLua]),
    (ENV.name, &[ConfigSource::ProjectEnv]),
    (WORKFLOWS_SUBDIR, &[ConfigSource::Workflows]),
    (AUTOMATIONS_SUBDIR, &[ConfigSource::Automations]),
];
/// Read under every project ancestor.
const PROJECT_TIERS: &[Tiers] = &[
    (PROJECT_SKILL_DIRS, ConfigSource::Skills),
    (PROJECT_COMMAND_DIRS, ConfigSource::Commands),
];
/// Read under the user's home.
const HOME_TIERS: &[Tiers] = &[
    (GLOBAL_SKILL_DIRS, ConfigSource::Skills),
    (GLOBAL_THIRD_PARTY_COMMAND_DIRS, ConfigSource::Commands),
];

/// A location under a base directory and the sources read from it.
type Rule = (&'static str, &'static [ConfigSource]);
/// A tier list of locations under a base directory, all read as one source.
type Tiers = (&'static [&'static str], ConfigSource);
type SharedWatcher = Arc<Mutex<Option<RecommendedWatcher>>>;

/// Something a consumer reloads as a unit, declared in processing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigSource {
    /// `caudra.toml`.
    Settings,
    Permissions,
    Mcp,
    Providers,
    /// The user-global permission question set override.
    DecisionQuestions,
    Workcell,
    Sandboxes,
    Skills,
    PromptProfiles,
    Commands,
    Workflows,
    Automations,
    /// `init.lua` and the modules it requires.
    InitLua,
    GlobalEnv,
    ProjectEnv,
}

impl ConfigSource {
    /// Read from whole directories, where any change counts, rather than from
    /// files whose bytes can be compared.
    const fn reads_directories(self) -> bool {
        match self {
            Self::Skills
            | Self::PromptProfiles
            | Self::Commands
            | Self::Workflows
            | Self::Automations
            | Self::InitLua => true,
            Self::Settings
            | Self::Permissions
            | Self::Mcp
            | Self::Providers
            | Self::DecisionQuestions
            | Self::Workcell
            | Self::Sandboxes
            | Self::GlobalEnv
            | Self::ProjectEnv => false,
        }
    }
}

/// The directories configuration is read under.
#[derive(Debug, Default)]
pub struct WatchRoots {
    /// The global config directory.
    pub global: Option<PathBuf>,
    /// Where the compatibility skill and command directories live.
    pub home: Option<PathBuf>,
    /// The working directory first, then each ancestor up to the repository
    /// root.
    pub project: Vec<PathBuf>,
}

impl WatchRoots {
    pub fn discover(cwd: &Path) -> Self {
        Self {
            global: paths::config_dir().ok(),
            home: paths::home(),
            project: find_project_ancestor_dirs(cwd).collect(),
        }
    }

    /// Spelled the way events are reported, which resolves symlinks on some
    /// platforms: FSEvents reports `/private/var` for `/var`.
    fn canonical(self) -> Self {
        Self {
            global: self.global.map(canonical),
            home: self.home.map(canonical),
            project: self.project.into_iter().map(canonical).collect(),
        }
    }
}

/// Which sources a change at `path` touches. A directory counts for every
/// source at, under, or beyond it, so one created whole is not missed while
/// its watch is still being placed. A root itself counts for nothing.
pub fn classify(path: &Path, roots: &WatchRoots) -> BTreeSet<ConfigSource> {
    let mut found = BTreeSet::new();
    if is_editor_temp(path) {
        return found;
    }
    if let Some(relative) = roots
        .global
        .as_deref()
        .and_then(|global| relative_to(path, global))
    {
        apply_rules(relative, GLOBAL_RULES, &mut found);
    }
    if let Some(relative) = roots
        .home
        .as_deref()
        .and_then(|home| relative_to(path, home))
    {
        apply_tiers(relative, HOME_TIERS, &mut found);
    }
    for (depth, ancestor) in roots.project.iter().enumerate() {
        let Some(relative) = relative_to(path, ancestor) else {
            continue;
        };
        apply_tiers(relative, PROJECT_TIERS, &mut found);
        if depth == 0
            && let Ok(inside) = relative.strip_prefix(PROJECT_DIR)
        {
            apply_rules(inside, PROJECT_RULES, &mut found);
        }
    }
    found
}

/// The files whose bytes decide `source`, from the same rules [`classify`]
/// reads, spelled as the loaders build them. A source read from directories
/// lists none.
pub fn source_files(source: ConfigSource, roots: &WatchRoots) -> Vec<PathBuf> {
    if source.reads_directories() {
        return Vec::new();
    }
    let global = roots
        .global
        .iter()
        .flat_map(|global| rule_files(global.clone(), GLOBAL_RULES, source));
    let project = roots
        .project
        .first()
        .into_iter()
        .flat_map(|cwd| rule_files(cwd.join(PROJECT_DIR), PROJECT_RULES, source));
    global.chain(project).collect()
}

fn rule_files(
    base: PathBuf,
    rules: &'static [Rule],
    source: ConfigSource,
) -> impl Iterator<Item = PathBuf> {
    rules
        .iter()
        .filter(move |(_, sources)| sources.contains(&source))
        .map(move |(location, _)| base.join(location))
}

fn relative_to<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
    path.strip_prefix(base)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
}

fn apply_rules(relative: &Path, rules: &[Rule], found: &mut BTreeSet<ConfigSource>) {
    for (location, sources) in rules {
        if overlaps(relative, Path::new(location)) {
            found.extend(sources.iter().copied());
        }
    }
}

fn apply_tiers(relative: &Path, tiers: &[Tiers], found: &mut BTreeSet<ConfigSource>) {
    for (locations, source) in tiers {
        if locations
            .iter()
            .any(|location| overlaps(relative, Path::new(location)))
        {
            found.insert(*source);
        }
    }
}

/// At or under `location`, or a directory on the way to it.
fn overlaps(relative: &Path, location: &Path) -> bool {
    relative.starts_with(location) || location.starts_with(relative)
}

fn is_editor_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == VIM_WRITE_PROBE
                || name.starts_with(TEMP_PREFIX)
                || TEMP_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        })
}

/// Holds changes back until they stop for [`QUIET`], but never longer than
/// [`MAX_DELAY`] after the first, so an editor saving in a loop still reloads.
#[derive(Debug, Default)]
pub struct Debouncer {
    pending: BTreeSet<ConfigSource>,
    /// When the first pending change landed, and the latest.
    window: Option<(Instant, Instant)>,
}

impl Debouncer {
    /// No sources is no change, and starts no clock.
    pub fn push(&mut self, sources: impl IntoIterator<Item = ConfigSource>, now: Instant) {
        let mut sources = sources.into_iter().peekable();
        if sources.peek().is_none() {
            return;
        }
        self.pending.extend(sources);
        let first = self.window.map_or(now, |(first, _)| first);
        self.window = Some((first, now));
    }

    /// The pending sources, once their deadline has passed.
    pub fn due(&mut self, now: Instant) -> Option<BTreeSet<ConfigSource>> {
        if self.deadline()? > now {
            return None;
        }
        self.window = None;
        Some(mem::take(&mut self.pending))
    }

    /// Time left until [`Self::due`] reports, or `None` with nothing pending.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn deadline(&self) -> Option<Instant> {
        self.window
            .map(|(first, latest)| (latest + QUIET).min(first + MAX_DELAY))
    }
}

/// Watches [`WatchRoots`] on a thread of its own and hands each settled set
/// of changed sources to a callback. Dropping it stops the thread.
pub struct ConfigWatcher {
    /// Shared with the thread, which places watches on directories as they
    /// appear. Taking it closes the event channel, which ends the thread.
    watcher: SharedWatcher,
    thread: Option<JoinHandle<()>>,
}

impl ConfigWatcher {
    /// Every watch is placed before this returns, so a change made after it
    /// is seen. A root that does not exist yet is skipped, and watched once
    /// it appears under a watched parent.
    pub fn spawn(
        roots: WatchRoots,
        on_change: impl Fn(BTreeSet<ConfigSource>) + Send + 'static,
    ) -> Result<Self, WatchError> {
        let roots = roots.canonical();
        let (sender, events) = flume::unbounded();
        let mut watcher = RecommendedWatcher::new(
            sender,
            Config::default().with_event_kinds(EventKindMask::CORE),
        )?;
        let targets = watch_targets(&roots);
        let watched = targets
            .iter()
            .filter(|(dir, mode)| place(&mut watcher, dir, *mode))
            .count();
        debug!(targets = targets.len(), watched, "config watch started");
        let watcher = Arc::new(Mutex::new(Some(watcher)));
        let shared = Arc::clone(&watcher);
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || run(&events, &shared, &roots, &targets, on_change))
            .map_err(WatchError::io)?;
        Ok(Self {
            watcher,
            thread: Some(thread),
        })
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        drop(
            self.watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!(thread = THREAD_NAME, "config watch thread panicked");
        }
    }
}

fn run(
    events: &Receiver<Watched<Event>>,
    watcher: &SharedWatcher,
    roots: &WatchRoots,
    targets: &[(PathBuf, RecursiveMode)],
    on_change: impl Fn(BTreeSet<ConfigSource>),
) {
    let mut debouncer = Debouncer::default();
    loop {
        let received = match debouncer.wait(Instant::now()) {
            Some(wait) => events.recv_timeout(wait),
            None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Ok(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                if appeared(&event.kind) {
                    rewatch(watcher, targets, &event.paths);
                }
                let now = Instant::now();
                for path in &event.paths {
                    debouncer.push(classify(path, roots), now);
                }
            }
            Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => warn!(%error, paths = ?error.paths, "config watch failed"),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(changed) = debouncer.due(Instant::now()) {
            on_change(changed);
        }
    }
}

fn appeared(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// The kernel drops a watch with its directory, and a directory made since
/// has none, so every target at or under one that appeared is placed again.
fn rewatch(watcher: &SharedWatcher, targets: &[(PathBuf, RecursiveMode)], appeared: &[PathBuf]) {
    let mut guard = watcher.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(watcher) = guard.as_mut() else {
        return;
    };
    for (dir, mode) in targets
        .iter()
        .filter(|(dir, _)| appeared.iter().any(|path| dir.starts_with(path)))
    {
        place(watcher, dir, *mode);
    }
}

/// Every directory worth a watch, present or not, and how deep. A project
/// ancestor, and each directory on the way to a location, is watched
/// shallowly only to see the next one appear. The home directory itself is
/// left alone, and nothing inside a recursive watch is listed again.
fn watch_targets(roots: &WatchRoots) -> Vec<(PathBuf, RecursiveMode)> {
    let mut targets = BTreeMap::new();
    if let Some(global) = &roots.global {
        targets.insert(global.clone(), RecursiveMode::Recursive);
    }
    for (depth, ancestor) in roots.project.iter().enumerate() {
        targets
            .entry(ancestor.clone())
            .or_insert(RecursiveMode::NonRecursive);
        for location in tier_locations(PROJECT_TIERS) {
            add_location(&mut targets, ancestor, ancestor.join(location));
        }
        if depth == 0 {
            let project = ancestor.join(PROJECT_DIR);
            for (location, _) in PROJECT_RULES {
                add_location(&mut targets, ancestor, project.join(location));
            }
        }
    }
    if let Some(home) = &roots.home {
        for location in tier_locations(HOME_TIERS) {
            add_location(&mut targets, home, home.join(location));
        }
    }
    let recursive: Vec<PathBuf> = targets
        .iter()
        .filter(|(_, mode)| **mode == RecursiveMode::Recursive)
        .map(|(dir, _)| dir.clone())
        .collect();
    targets
        .into_iter()
        .filter(|(dir, _)| {
            !recursive
                .iter()
                .any(|root| dir != root && dir.starts_with(root))
        })
        .collect()
}

fn tier_locations(tiers: &[Tiers]) -> impl Iterator<Item = &'static str> + '_ {
    tiers
        .iter()
        .flat_map(|(locations, _)| locations.iter().copied())
}

/// `target` recursively, and each directory between `base` and it shallowly.
fn add_location(targets: &mut BTreeMap<PathBuf, RecursiveMode>, base: &Path, target: PathBuf) {
    for between in target.ancestors().skip(1).take_while(|dir| *dir != base) {
        targets
            .entry(between.to_path_buf())
            .or_insert(RecursiveMode::NonRecursive);
    }
    targets.insert(target, RecursiveMode::Recursive);
}

/// Whether `dir` is now watched. One that is missing, or not a directory, is
/// skipped: it is placed when it appears.
fn place(watcher: &mut RecommendedWatcher, dir: &Path, mode: RecursiveMode) -> bool {
    if !dir.is_dir() {
        return false;
    }
    match watcher.watch(dir, mode) {
        Ok(()) => true,
        Err(error) if matches!(error.kind, ErrorKind::PathNotFound) => false,
        Err(error) => {
            warn!(path = %dir.display(), ?mode, %error, "config watch refused");
            false
        }
    }
}

fn canonical(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use caudra_config::PROJECT_DIR;
    use caudra_config::config_file::{CONFIG_FILE, INIT_LUA_FILE};
    use caudra_config::files::{ENV, PERMISSIONS, PROVIDERS, WORKCELL};
    use caudra_config::mcp::MCP_FILE;
    use caudra_config::sandbox::SANDBOX_FILE;
    use flume::Receiver;
    use notify::RecursiveMode;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        ConfigSource, ConfigWatcher, Debouncer, LUA_MODULES_DIR, MAX_DELAY, QUIET, VIM_WRITE_PROBE,
        WatchRoots, classify, source_files, watch_targets,
    };
    use crate::automation::catalog::AUTOMATIONS_SUBDIR;
    use crate::command::{COMMANDS_SUBDIR, GLOBAL_THIRD_PARTY_COMMAND_DIRS, PROJECT_COMMAND_DIRS};
    use crate::decisions::PERMISSION_QUESTION_FILE;
    use crate::prompt::profile::PROFILE_DIR;
    use crate::tools::native::skill::{GLOBAL_SKILL_DIRS, PROJECT_SKILL_DIRS, SKILLS_SUBDIR};
    use crate::workflow::catalog::WORKFLOWS_SUBDIR;

    /// Bounds a failure rather than pacing a success: a working watch answers
    /// within the debounce window and the test ends there.
    const DELIVERY_DEADLINE: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(1);

    const GLOBAL: &str = "global";
    const HOME: &str = "home";
    const REPO: &str = "repo";
    const NESTED: &str = "nested";
    const SKILL: &str = "review";
    const SKILL_FILE: &str = "SKILL.md";
    const SKILL_SWAP: &str = ".SKILL.md.swp";
    const COMMAND: &str = "review.md";
    const SCRIPT: &str = "review.rhai";
    const PROFILE: &str = "terse.md";
    const MODULE: &str = "helpers.lua";
    const UNRELATED: &str = "notes.txt";
    const CLAUDE_DIR: &str = ".claude";
    const SWAP: &str = "caudra.toml.swp";
    const BACKUP: &str = "caudra.toml~";
    const EMACS_LOCK: &str = ".#caudra.toml";
    const CONTENT: &str = "# edited\n";

    const WRONG_SOURCES: &str = "a path was classified as the wrong sources";
    const WRONG_TARGETS: &str = "a directory was watched at the wrong depth, or not at all";
    const WRONG_FILES: &str = "a source listed the wrong files behind it";
    const TOO_EAGER: &str = "a change was reported before its deadline";
    const HELD_BACK: &str = "a change past its deadline was not reported";
    const REPORTED_TWICE: &str = "a reported change was still pending";
    const NOT_DELIVERED: &str = "a change under a watched root never reached the callback";
    const STILL_RUNNING: &str = "dropping the watcher left its thread running";

    const GLOBAL_ENV_READERS: &[ConfigSource] = &[
        ConfigSource::GlobalEnv,
        ConfigSource::Settings,
        ConfigSource::Permissions,
        ConfigSource::Mcp,
        ConfigSource::Providers,
        ConfigSource::Workcell,
    ];
    const EVERY_PROJECT_SOURCE: &[ConfigSource] = &[
        ConfigSource::Settings,
        ConfigSource::Permissions,
        ConfigSource::Mcp,
        ConfigSource::Skills,
        ConfigSource::Commands,
        ConfigSource::Workflows,
        ConfigSource::Automations,
        ConfigSource::InitLua,
        ConfigSource::ProjectEnv,
    ];
    const SKILLS_AND_COMMANDS: &[ConfigSource] = &[ConfigSource::Skills, ConfigSource::Commands];

    enum Base {
        Global,
        Home,
        Cwd,
        Ancestor,
    }

    fn roots(dir: &Path) -> WatchRoots {
        let repo = dir.join(REPO);
        WatchRoots {
            global: Some(dir.join(GLOBAL)),
            home: Some(dir.join(HOME)),
            project: vec![repo.join(NESTED), repo],
        }
    }

    fn under(roots: &WatchRoots, base: Base, components: &[&str]) -> PathBuf {
        let root = match base {
            Base::Global => roots.global.clone(),
            Base::Home => roots.home.clone(),
            Base::Cwd => roots.project.first().cloned(),
            Base::Ancestor => roots.project.last().cloned(),
        };
        components
            .iter()
            .fold(root.unwrap(), |path, component| path.join(component))
    }

    fn sources(expected: &[ConfigSource]) -> BTreeSet<ConfigSource> {
        expected.iter().copied().collect()
    }

    fn sorted_files(source: ConfigSource, roots: &WatchRoots) -> Vec<PathBuf> {
        let mut files = source_files(source, roots);
        files.sort();
        files
    }

    /// `global` names files in the global directory, `project` in the working
    /// directory's `.caudra`.
    fn expected_files(roots: &WatchRoots, global: &[&str], project: &[&str]) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = global
            .iter()
            .map(|&name| under(roots, Base::Global, &[name]))
            .chain(
                project
                    .iter()
                    .map(|&name| under(roots, Base::Cwd, &[PROJECT_DIR, name])),
            )
            .collect();
        files.sort();
        files
    }

    fn spawn_reporting(roots: WatchRoots) -> (ConfigWatcher, Receiver<BTreeSet<ConfigSource>>) {
        let (sender, changes) = flume::unbounded();
        let watcher = ConfigWatcher::spawn(roots, move |changed| {
            let _ = sender.send(changed);
        })
        .unwrap();
        (watcher, changes)
    }

    #[test_case(Base::Global, &[CONFIG_FILE], &[ConfigSource::Settings]; "global_settings")]
    #[test_case(Base::Global, &[PERMISSIONS.name], &[ConfigSource::Permissions]; "global_permissions")]
    #[test_case(Base::Global, &[MCP_FILE], &[ConfigSource::Mcp]; "global_mcp")]
    #[test_case(Base::Global, &[PROVIDERS.name], &[ConfigSource::Providers]; "global_providers")]
    #[test_case(Base::Global, &[PERMISSION_QUESTION_FILE], &[ConfigSource::DecisionQuestions]; "global_permission_questions")]
    #[test_case(Base::Global, &[WORKCELL.name], &[ConfigSource::Workcell]; "global_workcell")]
    #[test_case(Base::Global, &[SANDBOX_FILE], &[ConfigSource::Sandboxes]; "global_sandboxes")]
    #[test_case(Base::Global, &[INIT_LUA_FILE], &[ConfigSource::InitLua]; "global_init_lua")]
    #[test_case(Base::Global, &[LUA_MODULES_DIR, MODULE], &[ConfigSource::InitLua]; "global_lua_module")]
    #[test_case(Base::Global, &[ENV.name], GLOBAL_ENV_READERS; "global_env_feeds_every_interpolating_file")]
    #[test_case(Base::Global, &[SKILLS_SUBDIR], &[ConfigSource::Skills]; "global_skills_dir_itself")]
    #[test_case(Base::Global, &[SKILLS_SUBDIR, SKILL, SKILL_FILE], &[ConfigSource::Skills]; "global_skill")]
    #[test_case(Base::Global, &[PROFILE_DIR, PROFILE], &[ConfigSource::PromptProfiles]; "global_prompt_profile")]
    #[test_case(Base::Global, &[COMMANDS_SUBDIR, COMMAND], &[ConfigSource::Commands]; "global_command")]
    #[test_case(Base::Global, &[WORKFLOWS_SUBDIR, SCRIPT], &[ConfigSource::Workflows]; "global_workflow")]
    #[test_case(Base::Global, &[AUTOMATIONS_SUBDIR, SCRIPT], &[ConfigSource::Automations]; "global_automation")]
    #[test_case(Base::Global, &[SWAP], &[]; "global_swap_file")]
    #[test_case(Base::Global, &[BACKUP], &[]; "global_backup_file")]
    #[test_case(Base::Global, &[VIM_WRITE_PROBE], &[]; "global_vim_write_probe")]
    #[test_case(Base::Global, &[EMACS_LOCK], &[]; "global_emacs_lock")]
    #[test_case(Base::Global, &[SKILLS_SUBDIR, SKILL, SKILL_SWAP], &[]; "swap_file_inside_a_tree")]
    #[test_case(Base::Global, &[UNRELATED], &[]; "global_unrelated_file")]
    #[test_case(Base::Global, &[], &[]; "global_dir_itself")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, CONFIG_FILE], &[ConfigSource::Settings]; "project_settings")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, PERMISSIONS.name], &[ConfigSource::Permissions]; "project_permissions")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, MCP_FILE], &[ConfigSource::Mcp]; "project_mcp")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, INIT_LUA_FILE], &[ConfigSource::InitLua]; "project_init_lua")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, LUA_MODULES_DIR, MODULE], &[ConfigSource::InitLua]; "project_lua_module")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, ENV.name], &[ConfigSource::ProjectEnv]; "project_env_feeds_nothing_else")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, WORKFLOWS_SUBDIR, SCRIPT], &[ConfigSource::Workflows]; "project_workflow")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, AUTOMATIONS_SUBDIR, SCRIPT], &[ConfigSource::Automations]; "project_automation")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, SKILLS_SUBDIR, SKILL, SKILL_FILE], &[ConfigSource::Skills]; "project_skill")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, COMMANDS_SUBDIR, COMMAND], &[ConfigSource::Commands]; "project_command")]
    #[test_case(Base::Cwd, &[PROJECT_DIR, PROVIDERS.name], &[]; "project_providers_are_not_read")]
    #[test_case(Base::Cwd, &[PROJECT_DIR], EVERY_PROJECT_SOURCE; "project_dir_itself")]
    #[test_case(Base::Cwd, &[CLAUDE_DIR], SKILLS_AND_COMMANDS; "compatibility_dir_itself")]
    #[test_case(Base::Cwd, &[UNRELATED], &[]; "project_unrelated_file")]
    #[test_case(Base::Cwd, &[], &[]; "working_directory_itself")]
    #[test_case(Base::Ancestor, &[PROJECT_DIR, CONFIG_FILE], &[]; "ancestor_settings_are_not_read")]
    #[test_case(Base::Ancestor, &[PROJECT_DIR, ENV.name], &[]; "ancestor_env_is_not_read")]
    #[test_case(Base::Ancestor, &[PROJECT_DIR, SKILLS_SUBDIR, SKILL, SKILL_FILE], &[ConfigSource::Skills]; "ancestor_skill")]
    #[test_case(Base::Ancestor, &[PROJECT_DIR, COMMANDS_SUBDIR, COMMAND], &[ConfigSource::Commands]; "ancestor_command")]
    #[test_case(Base::Ancestor, &[PROJECT_DIR], SKILLS_AND_COMMANDS; "ancestor_project_dir_itself")]
    #[test_case(Base::Home, &[UNRELATED], &[]; "home_unrelated_file")]
    fn a_path_classifies_as_the_sources_read_from_it(
        base: Base,
        components: &[&str],
        expected: &[ConfigSource],
    ) {
        let dir = TempDir::new().unwrap();
        let roots = roots(dir.path());
        assert_eq!(
            classify(&under(&roots, base, components), &roots),
            sources(expected),
            "{WRONG_SOURCES}"
        );
    }

    #[test_case(Base::Cwd, PROJECT_SKILL_DIRS, ConfigSource::Skills; "working_directory_skill_tiers")]
    #[test_case(Base::Ancestor, PROJECT_SKILL_DIRS, ConfigSource::Skills; "ancestor_skill_tiers")]
    #[test_case(Base::Cwd, PROJECT_COMMAND_DIRS, ConfigSource::Commands; "working_directory_command_tiers")]
    #[test_case(Base::Ancestor, PROJECT_COMMAND_DIRS, ConfigSource::Commands; "ancestor_command_tiers")]
    #[test_case(Base::Home, GLOBAL_SKILL_DIRS, ConfigSource::Skills; "home_skill_tiers")]
    #[test_case(Base::Home, GLOBAL_THIRD_PARTY_COMMAND_DIRS, ConfigSource::Commands; "home_command_tiers")]
    fn every_tier_directory_and_its_entries_classify(
        base: Base,
        tiers: &[&str],
        expected: ConfigSource,
    ) {
        let dir = TempDir::new().unwrap();
        let roots = roots(dir.path());
        let root = under(&roots, base, &[]);
        for tier in tiers {
            let tier = root.join(tier);
            for path in [tier.clone(), tier.join(SKILL).join(SKILL_FILE)] {
                assert_eq!(
                    classify(&path, &roots),
                    sources(&[expected]),
                    "{WRONG_SOURCES}: {}",
                    path.display()
                );
            }
        }
    }

    #[test_case(ConfigSource::Settings, &[CONFIG_FILE, ENV.name], &[CONFIG_FILE]; "settings")]
    #[test_case(ConfigSource::Permissions, &[PERMISSIONS.name, ENV.name], &[PERMISSIONS.name]; "permissions")]
    #[test_case(ConfigSource::Mcp, &[MCP_FILE, ENV.name], &[MCP_FILE]; "mcp")]
    #[test_case(ConfigSource::Providers, &[PROVIDERS.name, ENV.name], &[]; "providers")]
    #[test_case(ConfigSource::DecisionQuestions, &[PERMISSION_QUESTION_FILE], &[]; "permission_questions")]
    #[test_case(ConfigSource::Workcell, &[WORKCELL.name, ENV.name], &[]; "workcell")]
    #[test_case(ConfigSource::Sandboxes, &[SANDBOX_FILE], &[]; "sandboxes_are_read_literally")]
    #[test_case(ConfigSource::GlobalEnv, &[ENV.name], &[]; "global_env")]
    #[test_case(ConfigSource::ProjectEnv, &[], &[ENV.name]; "project_env")]
    fn a_file_backed_source_lists_the_files_it_reads(
        source: ConfigSource,
        global: &[&str],
        project: &[&str],
    ) {
        let dir = TempDir::new().unwrap();
        let roots = roots(dir.path());
        assert_eq!(
            sorted_files(source, &roots),
            expected_files(&roots, global, project),
            "{WRONG_FILES}"
        );
    }

    #[test_case(ConfigSource::Skills; "skills")]
    #[test_case(ConfigSource::PromptProfiles; "prompt_profiles")]
    #[test_case(ConfigSource::Commands; "commands")]
    #[test_case(ConfigSource::Workflows; "workflows")]
    #[test_case(ConfigSource::Automations; "automations")]
    #[test_case(ConfigSource::InitLua; "init_lua")]
    fn a_directory_source_lists_no_files(source: ConfigSource) {
        let dir = TempDir::new().unwrap();
        assert!(
            source_files(source, &roots(dir.path())).is_empty(),
            "{WRONG_FILES}"
        );
    }

    #[test_case(ConfigSource::Settings, &[CONFIG_FILE]; "settings")]
    #[test_case(ConfigSource::Mcp, &[MCP_FILE]; "mcp")]
    #[test_case(ConfigSource::Providers, &[]; "providers")]
    #[test_case(ConfigSource::GlobalEnv, &[]; "global_env")]
    #[test_case(ConfigSource::ProjectEnv, &[ENV.name]; "project_env")]
    fn a_missing_global_root_lists_only_project_files(source: ConfigSource, project: &[&str]) {
        let dir = TempDir::new().unwrap();
        let roots = WatchRoots {
            global: None,
            ..roots(dir.path())
        };
        assert_eq!(
            sorted_files(source, &roots),
            expected_files(&roots, &[], project),
            "{WRONG_FILES}"
        );
    }

    #[test]
    fn ancestors_are_watched_shallowly_and_config_trees_recursively() {
        let dir = TempDir::new().unwrap();
        let roots = roots(dir.path());
        let targets: BTreeMap<_, _> = watch_targets(&roots).into_iter().collect();
        let cwd = under(&roots, Base::Cwd, &[]);
        let expected = [
            (
                under(&roots, Base::Global, &[]),
                Some(RecursiveMode::Recursive),
            ),
            (under(&roots, Base::Home, &[]), None),
            (cwd.clone(), Some(RecursiveMode::NonRecursive)),
            (cwd.join(PROJECT_DIR), Some(RecursiveMode::NonRecursive)),
            (
                cwd.join(PROJECT_DIR).join(WORKFLOWS_SUBDIR),
                Some(RecursiveMode::Recursive),
            ),
            (
                under(&roots, Base::Ancestor, &[PROJECT_DIR, WORKFLOWS_SUBDIR]),
                None,
            ),
        ];
        for (dir, mode) in expected {
            assert_eq!(
                targets.get(&dir).copied(),
                mode,
                "{WRONG_TARGETS}: {}",
                dir.display()
            );
        }
    }

    #[test]
    fn nothing_inside_a_recursive_watch_is_listed_again() {
        let dir = TempDir::new().unwrap();
        let global = dir.path().join(GLOBAL);
        let roots = WatchRoots {
            global: Some(global.clone()),
            project: vec![global.join(REPO)],
            ..WatchRoots::default()
        };
        assert_eq!(
            watch_targets(&roots),
            vec![(global, RecursiveMode::Recursive)],
            "{WRONG_TARGETS}"
        );
    }

    #[test]
    fn nothing_pending_waits_for_nothing() {
        let now = Instant::now();
        let mut debouncer = Debouncer::default();
        debouncer.push([], now);
        assert_eq!(debouncer.wait(now), None, "{TOO_EAGER}");
        assert_eq!(debouncer.due(now + MAX_DELAY), None, "{TOO_EAGER}");
    }

    #[test]
    fn a_change_is_held_until_the_sources_go_quiet() {
        let start = Instant::now();
        let later = start + QUIET / 2;
        let mut debouncer = Debouncer::default();
        debouncer.push([ConfigSource::Settings], start);
        assert_eq!(debouncer.wait(start), Some(QUIET), "{TOO_EAGER}");
        assert_eq!(debouncer.due(later), None, "{TOO_EAGER}");

        debouncer.push([ConfigSource::Settings], later);
        assert_eq!(debouncer.due(start + QUIET), None, "{TOO_EAGER}");
        assert_eq!(
            debouncer.wait(start + QUIET),
            Some(QUIET / 2),
            "{TOO_EAGER}"
        );

        assert_eq!(
            debouncer.due(later + QUIET),
            Some(sources(&[ConfigSource::Settings])),
            "{HELD_BACK}"
        );
        assert_eq!(debouncer.due(later + QUIET * 2), None, "{REPORTED_TWICE}");
        assert_eq!(debouncer.wait(later + QUIET * 2), None, "{REPORTED_TWICE}");
    }

    #[test]
    fn a_file_rewritten_continuously_still_reloads_after_the_cap() {
        let start = Instant::now();
        let mut debouncer = Debouncer::default();
        let mut now = start;
        while now < start + MAX_DELAY {
            debouncer.push([ConfigSource::Mcp], now);
            assert_eq!(debouncer.due(now), None, "{TOO_EAGER}");
            now += QUIET / 2;
        }
        assert_eq!(debouncer.due(start + MAX_DELAY - TICK), None, "{TOO_EAGER}");
        assert_eq!(
            debouncer.due(start + MAX_DELAY),
            Some(sources(&[ConfigSource::Mcp])),
            "{HELD_BACK}"
        );
    }

    #[test]
    fn changes_in_one_window_coalesce() {
        let start = Instant::now();
        let mut debouncer = Debouncer::default();
        debouncer.push([ConfigSource::Settings], start);
        debouncer.push([ConfigSource::Mcp, ConfigSource::Settings], start + TICK);
        debouncer.push([], start + TICK * 2);
        assert_eq!(
            debouncer.due(start + TICK + QUIET),
            Some(sources(&[ConfigSource::Settings, ConfigSource::Mcp])),
            "{HELD_BACK}"
        );
    }

    #[test]
    fn a_global_write_is_reported_and_drop_stops_the_thread() {
        let dir = TempDir::new().unwrap();
        let global = dir.path().join(GLOBAL);
        fs::create_dir(&global).unwrap();
        let (watcher, changes) = spawn_reporting(WatchRoots {
            global: Some(global.clone()),
            ..WatchRoots::default()
        });

        fs::write(global.join(CONFIG_FILE), CONTENT).unwrap();
        let changed = changes
            .recv_timeout(DELIVERY_DEADLINE)
            .expect(NOT_DELIVERED);
        assert!(changed.contains(&ConfigSource::Settings), "{WRONG_SOURCES}");

        drop(watcher);
        assert!(changes.is_disconnected(), "{STILL_RUNNING}");
    }

    #[test]
    fn a_project_dir_created_after_spawn_is_watched() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join(REPO);
        fs::create_dir(&cwd).unwrap();
        let (_watcher, changes) = spawn_reporting(WatchRoots {
            project: vec![cwd.clone()],
            ..WatchRoots::default()
        });

        let project = cwd.join(PROJECT_DIR);
        fs::create_dir(&project).unwrap();
        let created = changes
            .recv_timeout(DELIVERY_DEADLINE)
            .expect(NOT_DELIVERED);
        assert_eq!(created, sources(EVERY_PROJECT_SOURCE), "{WRONG_SOURCES}");

        fs::write(project.join(MCP_FILE), CONTENT).unwrap();
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        while changes.recv_deadline(deadline).expect(NOT_DELIVERED) != sources(&[ConfigSource::Mcp])
        {
        }
    }
}
