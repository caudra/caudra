//! Applies configuration files edited while Caudra runs. What the host
//! enforces follows the files at once; what the model is shown changes at its
//! next free moment, and until then reaches it as a reminder.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_agent::McpConfigErrors;
use caudra_agent::command::{CustomCommand, discover_commands};
use caudra_agent::config_watch::{ConfigSource, ConfigWatcher, WatchRoots, source_files};
use caudra_agent::decisions::{Decisions, PERMISSION_QUESTION_FILE, resolve_active_decisions};
use caudra_agent::mcp::config::try_load_config;
use caudra_agent::mcp::start_with_config;
use caudra_agent::prompt::profile::{PROFILE_EXTENSION, PromptProfileCatalog};
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::skill;
use caudra_config::config_file::{global_config_path, project_config_path};
use caudra_config::decisions::DecisionsConfig;
use caudra_config::own_writes::{self, fingerprint};
use caudra_config::providers::ProvidersConfig;
use caudra_config::websearch::WebsearchConfig;
use caudra_config::{Config, PermissionsConfig, fail_closed_permissions, try_load_permissions};
use toml::Table;
use tracing::{debug, info, warn};

use super::EventLoop;
use crate::agent::Settings;
use crate::app::decisions::rebuilt_decisions;

const SEPARATOR: &str = " · ";
const LIST_SEPARATOR: &str = ", ";
const RELOADED: &str = "Reloaded";
const SETTINGS_FILE: &str = "caudra.toml";
const PERMISSIONS_FILE: &str = "permissions.toml";
const PROVIDERS_FILE: &str = "providers.toml";
const MCP_FILE: &str = "mcp.toml";
const SKILLS_LABEL: &str = "skills";
const PROMPT_PROFILES_LABEL: &str = "system prompts";
const COMMANDS_LABEL: &str = "commands";
const SETTINGS_KEPT: &str = "keeping the last valid settings";
const PROVIDERS_KEPT: &str = "keeping the last valid providers";
const MCP_KEPT: &str = "keeping the running servers";
const PROFILES_KEPT: &str = "a session using one keeps the version it loaded";
const PERMISSIONS_FAILED_CLOSED: &str = "every tool call is denied until the file loads again";
const PERMISSIONS_REVIEW: &str = "project rules wait for review in /permissions";
const MCP_ERROR_PREFIX: &str = "MCP config error: ";
const DECISION_ENGINE: &str = "decision engine";
const DECISION_ENGINE_KEPT: &str = "keeping the running decision engine";
const DECISIONS_TURNED_OFF: &str =
    "the active decision endpoint is gone from providers.toml, so decisions are off";
/// The tables every session uses as soon as the file is saved.
const NOW_TABLES: [&str; 2] = ["decisions", WEBSEARCH_TABLE];
const WEBSEARCH_TABLE: &str = "websearch";
const WEBSEARCH_REMOTE: &str =
    "[websearch] does not apply here: web tools run on the remote Workcell";
/// Keys that apply as soon as the file is saved, in a table whose other keys wait. A key
/// may be a dotted path into the table.
const NOW_KEYS: [(&str, &str); 2] = [("ui", "show_decisions"), ("plugins", "skill.pstack")];
const KEY_PATH_SEPARATOR: char = '.';
/// The tables whose settings a session adopts at its next turn.
const LIVE_TABLES: [&str; 2] = ["agent", "autopilot"];
/// The table read once, at startup.
const STARTUP_TABLE: &str = "experimental";
const NOW_SUFFIX: &str = "at once";
const LIVE_SUFFIX: &str = "from the next turn";
const RELOAD_SUFFIX: &str = "after /reload";
const RESTART_SUFFIX: &str = "after a restart";
const INIT_LUA_CHANGED: &str = "init.lua changed; /reload runs it again";
const WORKCELL_CHANGED: &str = "workcell.toml changed; /reload to apply it";
const SANDBOXES_CHANGED: &str = "sandboxes.toml changed; /reload to apply it";
const GLOBAL_ENV_CHANGED: &str = "the global .env changed: {env:NAME} references in config files follow it, but shell commands and keys read from the environment keep their startup values until Caudra restarts";
const PROJECT_ENV_CHANGED: &str = "the project .env changed; restart Caudra to apply it";
const REMOTE_PROJECT: &str = "project files of a remote workspace apply after /reload";
/// Sources read from files, as opposed to directories, so their bytes tell
/// an edit from a save that changed nothing.
const FILE_SOURCES: [ConfigSource; 9] = [
    ConfigSource::Settings,
    ConfigSource::Permissions,
    ConfigSource::Mcp,
    ConfigSource::Providers,
    ConfigSource::DecisionQuestions,
    ConfigSource::Workcell,
    ConfigSource::Sandboxes,
    ConfigSource::GlobalEnv,
    ConfigSource::ProjectEnv,
];

/// Re-reads the settings layers in a directory, merging the `init.lua` layers
/// recorded at startup rather than running the scripts again.
pub type SettingsReloader = Arc<dyn Fn(&Path) -> Result<Config, String> + Send + Sync>;

/// How a source's files changed since it was last applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Someone other than Caudra edited them, so the user hears about it.
    Edited,
    /// Caudra wrote them itself, so they apply without a word.
    Own,
}

/// What each watched file held when it was last applied: its content
/// fingerprint, or `None` while it does not exist.
struct Fingerprints(HashMap<PathBuf, Option<u64>>);

/// One file as it reads now.
struct Reading {
    print: Option<u64>,
    own: bool,
}

impl Reading {
    fn of(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => Self {
                print: Some(fingerprint(&bytes)),
                own: own_writes::is_own(path, &bytes),
            },
            Err(_) => Self {
                print: None,
                own: false,
            },
        }
    }
}

impl Fingerprints {
    fn read(roots: &WatchRoots) -> Self {
        Self(
            FILE_SOURCES
                .iter()
                .flat_map(|source| source_files(*source, roots))
                .map(|path| {
                    let print = Reading::of(&path).print;
                    (path, print)
                })
                .collect(),
        )
    }

    /// The sources in `sources` whose files changed, and how. A directory
    /// source always counts as edited. Every file is read once, so a file that
    /// several sources share, such as the global `.env`, counts for each.
    fn triage(
        &mut self,
        sources: &BTreeSet<ConfigSource>,
        roots: &WatchRoots,
    ) -> BTreeMap<ConfigSource, Change> {
        let mut readings: HashMap<PathBuf, Reading> = HashMap::new();
        let mut changes = BTreeMap::new();
        for &source in sources {
            let files = source_files(source, roots);
            if files.is_empty() {
                changes.insert(source, Change::Edited);
                continue;
            }
            let mut change = None;
            for path in files {
                let reading = readings
                    .entry(path.clone())
                    .or_insert_with(|| Reading::of(&path));
                if self.0.get(&path) == Some(&reading.print) {
                    continue;
                }
                change = Some(match change {
                    Some(Change::Edited) => Change::Edited,
                    _ if reading.own => Change::Own,
                    _ => Change::Edited,
                });
            }
            if let Some(change) = change {
                changes.insert(source, change);
            }
        }
        self.0.extend(
            readings
                .into_iter()
                .map(|(path, reading)| (path, reading.print)),
        );
        changes
    }
}

/// Both `caudra.toml` files as last applied, so a reload can name the tables
/// an edit touched without showing a value.
struct SettingsTables {
    global: Table,
    project: Table,
}

impl SettingsTables {
    /// A file that is missing or does not parse reads as empty; the settings
    /// reload reports why.
    fn read(roots: &WatchRoots) -> Self {
        let table = |path: Option<PathBuf>| {
            path.and_then(|path| fs::read_to_string(path).ok())
                .and_then(|text| text.parse::<Table>().ok())
                .unwrap_or_default()
        };
        Self {
            global: table(roots.global.as_deref().map(global_config_path)),
            project: table(roots.project.first().map(|cwd| project_config_path(cwd))),
        }
    }

    /// The top-level keys whose values differ in either file, with each of
    /// [`NOW_KEYS`] named as `table.key` apart from the rest of its table.
    /// The project file counts only when `project` is read at all.
    fn changed_keys(&self, after: &Self, project: bool) -> BTreeMap<String, bool> {
        let mut changed = BTreeMap::new();
        let mut compare = |before: &Table, after: &Table| {
            for key in before.keys().chain(after.keys()) {
                let (mut old, mut new) = (before.get(key).cloned(), after.get(key).cloned());
                for (table, now_key) in NOW_KEYS.iter().filter(|(table, _)| table == key) {
                    let take = |value: &mut Option<toml::Value>| {
                        let (parents, leaf) = now_key
                            .rsplit_once(KEY_PATH_SEPARATOR)
                            .map_or((None, *now_key), |(parents, leaf)| (Some(parents), leaf));
                        let mut table = value.as_mut().and_then(toml::Value::as_table_mut)?;
                        for parent in parents
                            .into_iter()
                            .flat_map(|path| path.split(KEY_PATH_SEPARATOR))
                        {
                            table = table.get_mut(parent).and_then(toml::Value::as_table_mut)?;
                        }
                        table.remove(leaf)
                    };
                    if take(&mut old) != take(&mut new) {
                        changed.insert(format!("{table}.{now_key}"), false);
                    }
                }
                if old != new {
                    let table = old.or(new).is_some_and(|value| value.is_table());
                    changed.insert(key.clone(), table);
                }
            }
        };
        compare(&self.global, &after.global);
        if project {
            compare(&self.project, &after.project);
        }
        changed
    }
}

/// When each changed key applies, by name only: `[decisions]`, `[websearch]` and `ui.show_decisions` at
/// once, `[agent]` and `[autopilot]` from the next turn, `[experimental]` after a restart,
/// everything else after `/reload`. `None` when no key changed, as for an edit to a comment.
fn settings_note(changed: &BTreeMap<String, bool>) -> Option<String> {
    let label = |key: &str, table: bool| {
        if table {
            format!("[{key}]")
        } else {
            key.to_owned()
        }
    };
    let applies_now = |key: &str| {
        NOW_TABLES.contains(&key)
            || key
                .split_once('.')
                .is_some_and(|split| NOW_KEYS.contains(&split))
    };
    let mut now = Vec::new();
    let mut live = Vec::new();
    let mut reload = Vec::new();
    let mut restart = Vec::new();
    for (key, &table) in changed {
        let bucket = match key.as_str() {
            key if applies_now(key) => &mut now,
            key if LIVE_TABLES.contains(&key) => &mut live,
            STARTUP_TABLE => &mut restart,
            _ => &mut reload,
        };
        bucket.push(label(key, table));
    }
    let parts: Vec<String> = [
        (now, NOW_SUFFIX),
        (live, LIVE_SUFFIX),
        (reload, RELOAD_SUFFIX),
        (restart, RESTART_SUFFIX),
    ]
    .into_iter()
    .filter(|(keys, _)| !keys.is_empty())
    .map(|(keys, suffix)| format!("{} {suffix}", keys.join(LIST_SEPARATOR)))
    .collect();
    (!parts.is_empty()).then(|| parts.join(LIST_SEPARATOR))
}

/// The settings `current` runs with, the defaults while none is installed.
fn running_decisions(current: Option<&Decisions>) -> DecisionsConfig {
    current.map_or_else(DecisionsConfig::default, |decisions| {
        decisions.config().clone()
    })
}

/// Each profile that failed to load, as `{name}.md: {reason}`. `None` once
/// every one loads.
fn profile_problem(catalog: &PromptProfileCatalog) -> Option<String> {
    let broken: Vec<String> = catalog
        .invalid()
        .map(|(name, reason)| format!("{name}.{PROFILE_EXTENSION}: {reason}"))
        .collect();
    (!broken.is_empty()).then(|| broken.join(LIST_SEPARATOR))
}

/// The watch on the files configuration is read from, how they read when
/// last applied, and how to read the settings again.
pub(super) struct ConfigReload {
    reloader: SettingsReloader,
    changed_tx: flume::Sender<BTreeSet<ConfigSource>>,
    changed_rx: flume::Receiver<BTreeSet<ConfigSource>>,
    watcher: Option<ConfigWatcher>,
    roots: WatchRoots,
    fingerprints: Fingerprints,
    tables: SettingsTables,
}

impl ConfigReload {
    pub(super) fn new(reloader: SettingsReloader, cwd: &Path) -> Self {
        let (changed_tx, changed_rx) = flume::unbounded();
        let watcher = watch(cwd, &changed_tx);
        let roots = WatchRoots::discover(cwd);
        Self {
            reloader,
            changed_tx,
            changed_rx,
            watcher,
            fingerprints: Fingerprints::read(&roots),
            tables: SettingsTables::read(&roots),
            roots,
        }
    }

    pub(super) fn changes(&self) -> &flume::Receiver<BTreeSet<ConfigSource>> {
        &self.changed_rx
    }

    /// Watches the project in `cwd` instead of the one before, taking its
    /// files as they read now.
    pub(super) fn rewatch(&mut self, cwd: &Path) {
        self.watcher = None;
        self.watcher = watch(cwd, &self.changed_tx);
        self.roots = WatchRoots::discover(cwd);
        self.fingerprints = Fingerprints::read(&self.roots);
        self.tables = SettingsTables::read(&self.roots);
    }

    /// The top-level `caudra.toml` keys that changed since the settings were
    /// last applied, which they now are.
    fn settings_changes(&mut self, project: bool) -> BTreeMap<String, bool> {
        let after = SettingsTables::read(&self.roots);
        let changed = self.tables.changed_keys(&after, project);
        self.tables = after;
        changed
    }
}

fn watch(cwd: &Path, changed: &flume::Sender<BTreeSet<ConfigSource>>) -> Option<ConfigWatcher> {
    let changed = changed.clone();
    ConfigWatcher::spawn(WatchRoots::discover(cwd), move |sources| {
        let _ = changed.send(sources);
    })
    .map_err(|error| warn!(%error, "config files are not watched; /reload applies edits"))
    .ok()
}

/// What one reload did, as the single line the user is shown.
#[derive(Default)]
struct Report {
    problems: Vec<String>,
    reloaded: Vec<&'static str>,
    notes: Vec<String>,
}

impl Report {
    fn note(&mut self, note: impl Into<String>) {
        let note = note.into();
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Takes `part` in, dropping what it says went well when Caudra made
    /// the change itself. Problems are always kept.
    fn merge(&mut self, part: Self, change: Change) {
        self.problems.extend(part.problems);
        if change == Change::Edited {
            self.reloaded.extend(part.reloaded);
            for note in part.notes {
                self.note(note);
            }
        }
    }

    fn render(&self) -> Option<String> {
        let mut parts = self.problems.clone();
        if !self.reloaded.is_empty() {
            parts.push(format!("{RELOADED} {}", self.reloaded.join(LIST_SEPARATOR)));
        }
        parts.extend(self.notes.iter().cloned());
        (!parts.is_empty()).then(|| parts.join(SEPARATOR))
    }
}

impl EventLoop<'_> {
    pub(super) fn apply_config_changes(&mut self, sources: BTreeSet<ConfigSource>) {
        let changes = self
            .config_reload
            .fingerprints
            .triage(&sources, &self.config_reload.roots);
        if changes.is_empty() {
            debug!(?sources, "config files unchanged");
            return;
        }
        let mut report = Report::default();
        let cwd = env::current_dir().ok();
        let cwd = cwd.as_deref();
        let local = self.ctx.workspace_session.is_none();
        let mut publish = false;
        for (&source, &change) in &changes {
            let mut part = Report::default();
            publish |= self.apply_source(source, cwd, local, &mut part);
            report.merge(part, change);
        }
        if publish {
            self.publish_settings();
        }
        info!(
            changes = ?changes,
            problems = report.problems.len(),
            "applied configuration changes"
        );
        if let Some(message) = report.render() {
            self.focused_app().flash(message);
        }
    }

    /// Applies one changed source, and says whether sessions need the
    /// settings published again.
    fn apply_source(
        &mut self,
        source: ConfigSource,
        cwd: Option<&Path>,
        local: bool,
        report: &mut Report,
    ) -> bool {
        match source {
            ConfigSource::Settings => return self.reload_settings(cwd, local, report),
            ConfigSource::PromptProfiles => {
                self.reload_prompt_profiles(report);
                report.reloaded.push(PROMPT_PROFILES_LABEL);
                return true;
            }
            ConfigSource::Mcp => return self.reload_mcp(cwd, report),
            ConfigSource::Permissions if local => {
                if let Some(cwd) = cwd {
                    self.reload_permissions(cwd, report);
                }
            }
            ConfigSource::Commands if local => {
                if let Some(cwd) = cwd {
                    self.reload_commands(cwd);
                    report.reloaded.push(COMMANDS_LABEL);
                }
            }
            ConfigSource::Permissions | ConfigSource::Commands => report.note(REMOTE_PROJECT),
            ConfigSource::Providers => {
                if self.check_providers(report) {
                    self.reload_decision_endpoint(report);
                }
            }
            ConfigSource::DecisionQuestions => {
                let current = self.ctx.permissions.decisions();
                let config = running_decisions(current.as_ref());
                if self.rebuild_decisions(current, config, true, report) {
                    report.reloaded.push(PERMISSION_QUESTION_FILE);
                }
            }
            ConfigSource::Skills => {
                skill::refresh(ToolRegistry::global());
                report.reloaded.push(SKILLS_LABEL);
            }
            ConfigSource::Workflows | ConfigSource::Automations => {}
            ConfigSource::Workcell => report.note(WORKCELL_CHANGED),
            ConfigSource::Sandboxes => report.note(SANDBOXES_CHANGED),
            ConfigSource::InitLua => report.note(INIT_LUA_CHANGED),
            ConfigSource::GlobalEnv => {
                report.note(GLOBAL_ENV_CHANGED);
                self.reapply_websearch(cwd, report);
            }
            ConfigSource::ProjectEnv => report.note(PROJECT_ENV_CHANGED),
        }
        false
    }

    /// Records the config files that failed to load when the TUI started,
    /// so the status bar shows them from the first frame.
    pub(super) fn check_config_files(&mut self, cwd: &Path) {
        if self.ctx.workspace_session.is_none() {
            let _ = self.checked_permissions(cwd);
        }
        if !self.ctx.mcp_config_errors.is_empty() {
            self.ctx.config_problems.set(
                MCP_FILE,
                self.ctx.mcp_config_errors.to_string(),
                MCP_KEPT,
            );
        }
        self.check_providers(&mut Report::default());
        self.check_prompt_profiles(&mut Report::default());
    }

    /// Records the profiles that failed to load, which a session keeps using
    /// as it loaded them, so the status bar shows them until they load again.
    fn check_prompt_profiles(&mut self, report: &mut Report) {
        match profile_problem(&self.ctx.prompt_profiles) {
            Some(error) => {
                report.problems.push(format!("{error}; {PROFILES_KEPT}"));
                self.ctx
                    .config_problems
                    .set(PROMPT_PROFILES_LABEL, error, PROFILES_KEPT);
            }
            None => self.ctx.config_problems.clear(PROMPT_PROFILES_LABEL),
        }
    }

    /// The permissions in `cwd`, recording a file that fails to load so the
    /// status bar shows it until it loads again. The caller denies every tool
    /// call meanwhile, so a broken edit never leaves a looser policy in force.
    pub(super) fn checked_permissions(&mut self, cwd: &Path) -> Result<PermissionsConfig, String> {
        let loaded = try_load_permissions(cwd);
        match &loaded {
            Ok(_) => self.ctx.config_problems.clear(PERMISSIONS_FILE),
            Err(error) => {
                warn!(%error, "permissions failed closed");
                self.ctx.config_problems.set(
                    PERMISSIONS_FILE,
                    error.clone(),
                    PERMISSIONS_FAILED_CLOSED,
                );
            }
        }
        loaded
    }

    /// Whether `providers.toml` loads.
    fn check_providers(&mut self, report: &mut Report) -> bool {
        match ProvidersConfig::check() {
            Ok(()) => {
                self.ctx.config_problems.clear(PROVIDERS_FILE);
                report.reloaded.push(PROVIDERS_FILE);
                true
            }
            Err(error) => {
                report.problems.push(format!("{error}; {PROVIDERS_KEPT}"));
                self.ctx
                    .config_problems
                    .set(PROVIDERS_FILE, error, PROVIDERS_KEPT);
                false
            }
        }
    }

    /// The active endpoint as `providers.toml` defines it now. A table that is gone turns
    /// decisions off, as at startup; one that does not resolve keeps the running engine.
    fn reload_decision_endpoint(&mut self, report: &mut Report) {
        let current = self.ctx.permissions.decisions();
        let running = running_decisions(current.as_ref());
        match resolve_active_decisions(&running, &self.ctx.storage, ProvidersConfig::load) {
            Ok(config) => {
                let turned_off = running.endpoint.is_some() && config.endpoint.is_none();
                if self.rebuild_decisions(current, config, false, report) && turned_off {
                    report.note(DECISIONS_TURNED_OFF);
                }
            }
            Err(error) => {
                warn!(%error, "active decision endpoint does not resolve");
                report
                    .problems
                    .push(format!("{error}; {DECISION_ENGINE_KEPT}"));
            }
        }
    }

    /// Every session screens with an engine built from `config`, unless the running one
    /// already is and nothing asks to `reread` its files. Whether it was rebuilt; a failure
    /// keeps the running engine.
    fn rebuild_decisions(
        &mut self,
        current: Option<Decisions>,
        config: DecisionsConfig,
        reread: bool,
        report: &mut Report,
    ) -> bool {
        match rebuilt_decisions(current.as_ref(), config, reread, &self.ctx.storage) {
            None => false,
            Some(Ok(decisions)) => {
                self.ctx.config_problems.clear(DECISION_ENGINE);
                self.install_decisions(&decisions);
                info!(
                    endpoint = decisions.endpoint().map(|endpoint| endpoint.name.as_str()),
                    reread, "decision engine rebuilt"
                );
                true
            }
            Some(Err(error)) => {
                warn!(%error, "decision engine rebuild failed");
                report
                    .problems
                    .push(format!("{error}; {DECISION_ENGINE_KEPT}"));
                self.ctx.config_problems.set(
                    DECISION_ENGINE,
                    error.to_string(),
                    DECISION_ENGINE_KEPT,
                );
                false
            }
        }
    }

    /// `ui.show_decisions` as saved, in every session.
    fn apply_show_decisions(&mut self, shown: bool) {
        if self.ctx.ui_config.show_decisions == shown {
            return;
        }
        self.ctx.ui_config.show_decisions = shown;
        for runtime in &mut self.sessions {
            runtime.app.set_show_decisions(shown);
        }
    }

    /// Whether settings were read; an invalid file keeps the ones in force.
    fn reload_settings(&mut self, cwd: Option<&Path>, local: bool, report: &mut Report) -> bool {
        let Some(cwd) = cwd else {
            return false;
        };
        match (self.config_reload.reloader)(cwd) {
            Ok(config) => {
                self.ctx.config_problems.clear(SETTINGS_FILE);
                let pstack_switched =
                    self.ctx.config.builtin_skills.pstack != config.agent.builtin_skills.pstack;
                self.ctx.config = config.agent;
                if pstack_switched {
                    self.apply_pstack(cwd, local, report);
                }
                for runtime in &self.sessions {
                    runtime
                        .app
                        .state
                        .autopilot
                        .set_limits(config.autopilot.clone());
                }
                self.ctx.autopilot = config.autopilot;
                let current = self.ctx.permissions.decisions();
                let endpoint = current
                    .as_ref()
                    .and_then(|decisions| decisions.endpoint().cloned());
                self.rebuild_decisions(
                    current,
                    config.decisions.with_endpoint(endpoint),
                    false,
                    report,
                );
                self.apply_show_decisions(config.ui.show_decisions);
                let changed = self.config_reload.settings_changes(local);
                if changed.contains_key(WEBSEARCH_TABLE) || self.ctx.websearch.is_some() {
                    self.apply_websearch(&config.websearch, report);
                }
                if let Some(note) = settings_note(&changed) {
                    report.reloaded.push(SETTINGS_FILE);
                    report.note(note);
                }
                true
            }
            Err(error) => {
                warn!(%error, "settings reload failed");
                report.problems.push(format!("{error}; {SETTINGS_KEPT}"));
                self.ctx
                    .config_problems
                    .set(SETTINGS_FILE, error, SETTINGS_KEPT);
                false
            }
        }
    }

    /// `[websearch]` as saved, on the local Workcell's `websearch` tool. A save that leaves every
    /// setting it reads alone says nothing, and keeps searches prepared before it valid.
    fn apply_websearch(&mut self, config: &WebsearchConfig, report: &mut Report) {
        let Some(control) = &self.ctx.websearch else {
            report.note(WEBSEARCH_REMOTE);
            return;
        };
        let applied = control.apply(config);
        if applied.changed {
            report.problems.extend(applied.warnings());
            report.note(applied.summary());
        }
    }

    /// The global `.env` changed, which `{env:NAME}` references in `[websearch]` follow.
    fn reapply_websearch(&mut self, cwd: Option<&Path>, report: &mut Report) {
        if self.ctx.websearch.is_none() {
            return;
        }
        let Some(cwd) = cwd else {
            return;
        };
        if let Ok(config) = (self.config_reload.reloader)(cwd) {
            self.apply_websearch(&config.websearch, report);
        }
    }

    /// `builtin_skills.pstack` turned on or off: the skill and bundled automations the reloader
    /// installed or removed, which each automation runtime lists at its next scan, the
    /// `pstack-*` profiles, and the `/pstack:` commands of every tab, local or remote.
    fn apply_pstack(&mut self, cwd: &Path, local: bool, report: &mut Report) {
        skill::refresh(ToolRegistry::global());
        self.reload_prompt_profiles(report);
        if local {
            self.reload_commands(cwd);
        }
        let pstack = self.ctx.config.builtin_skills.pstack;
        for runtime in &mut self.sessions {
            runtime.app.set_pstack(pstack);
        }
    }

    /// A session keeps a profile that is gone from disk, or broken, as it was
    /// loaded. Every tab takes the rest, so an agent it spawns later builds
    /// from what is on disk now.
    fn reload_prompt_profiles(&mut self, report: &mut Report) {
        let catalog = Arc::new(PromptProfileCatalog::discover_user(
            self.ctx.config.builtin_skills.pstack,
        ));
        catalog.refresh(&mut self.ctx.default_prompt_profile);
        for runtime in &mut self.sessions {
            runtime.app.adopt_prompt_profiles(&catalog);
        }
        self.ctx.prompt_profiles = catalog;
        self.check_prompt_profiles(report);
    }

    fn publish_settings(&mut self) {
        for runtime in &mut self.sessions {
            runtime.handles.publish_settings(Settings {
                config: self.ctx.config.clone(),
                prompt_profiles: Arc::clone(&self.ctx.prompt_profiles),
                mcp: self.ctx.mcp_handle.clone(),
            });
        }
    }

    /// Project allows that changed wait for review, which the status bar
    /// offers rather than opening a dialog mid-task.
    fn reload_permissions(&mut self, cwd: &Path, report: &mut Report) {
        let permissions = match self.checked_permissions(cwd) {
            Ok(permissions) => {
                report.reloaded.push(PERMISSIONS_FILE);
                permissions
            }
            Err(error) => {
                report
                    .problems
                    .push(format!("{error}; {PERMISSIONS_FAILED_CLOSED}"));
                fail_closed_permissions()
            }
        };
        self.ctx
            .permissions
            .set_project_with_config(cwd, permissions.clone());
        for runtime in &mut self.sessions {
            runtime.app.reload_permissions(cwd, permissions.clone());
        }
        if self.ctx.permissions.needs_project_permission_config_trust() {
            report.note(PERMISSIONS_REVIEW);
        }
    }

    fn reload_commands(&mut self, cwd: &Path) {
        let commands: Arc<[CustomCommand]> = if self.ctx.no_commands {
            Arc::from([])
        } else {
            Arc::from(discover_commands(
                cwd,
                self.ctx.config.builtin_skills.pstack,
            ))
        };
        for runtime in &mut self.sessions {
            runtime.app.reload_commands(Arc::clone(&commands));
        }
        self.ctx.custom_commands = commands;
    }

    /// An invalid file keeps every server running as it was. When startup
    /// found no servers, the first valid file with one starts a manager, and
    /// whether the sessions need the settings published again to adopt it.
    fn reload_mcp(&mut self, cwd: Option<&Path>, report: &mut Report) -> bool {
        let Some(cwd) = cwd else {
            return false;
        };
        let config = match try_load_config(cwd) {
            Ok(config) => config,
            Err(errors) => {
                warn!(%errors, "mcp config reload failed");
                let error = errors.to_string();
                report
                    .problems
                    .push(format!("{MCP_ERROR_PREFIX}{error}; {MCP_KEPT}"));
                self.ctx.config_problems.set(MCP_FILE, error, MCP_KEPT);
                self.show_mcp_errors(errors);
                return false;
            }
        };
        self.ctx.config_problems.clear(MCP_FILE);
        self.show_mcp_errors(McpConfigErrors::new(cwd.to_path_buf()));
        if let Some(handle) = &self.ctx.mcp_handle {
            handle.reload(config);
            report.reloaded.push(MCP_FILE);
            return false;
        }
        let Some(handle) = start_with_config(config) else {
            return false;
        };
        for runtime in &mut self.sessions {
            runtime.app.adopt_mcp_reader(handle.reader());
        }
        self.ctx.mcp_handle = Some(handle);
        report.reloaded.push(MCP_FILE);
        true
    }

    fn show_mcp_errors(&mut self, errors: McpConfigErrors) {
        for runtime in &mut self.sessions {
            runtime.app.set_mcp_config_errors(errors.clone());
        }
        self.ctx.mcp_config_errors = errors;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    use caudra_agent::config_watch::{ConfigSource, WatchRoots};
    use caudra_agent::prompt::profile::{PROFILE_DIR, PROFILE_EXTENSION, PromptProfileCatalog};
    use caudra_config::own_writes;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Change, Fingerprints, LIST_SEPARATOR, LIVE_SUFFIX, NOW_SUFFIX, RELOAD_SUFFIX, RELOADED,
        RESTART_SUFFIX, Report, SEPARATOR, SETTINGS_FILE, SettingsTables, WEBSEARCH_TABLE,
        profile_problem, settings_note,
    };

    const PROBLEM: &str = "providers.toml: expected `=`";
    const FIRST: &str = "skills";
    const SECOND: &str = "commands";
    const NOTE: &str = "init.lua changed";
    const SETTINGS: &str = "[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"dark\"\n";
    const COMMENTED: &str = "# tuned\n[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"dark\"\n";
    const AGENT_EDITED: &str = "[agent]\nmax_output_lines = 20\n\n[ui]\ntheme = \"dark\"\n";
    const UI_AND_FLAG_EDITED: &str =
        "always_yolo = true\n[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"light\"\n";
    const SHOW_DECISIONS_EDITED: &str =
        "[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"dark\"\nshow_decisions = false\n";
    const UI_AND_SHOW_DECISIONS_EDITED: &str =
        "[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"light\"\nshow_decisions = false\n";
    const PSTACK_OFF: &str = "[plugins.skill]\npstack = false\n";
    const PSTACK_ON: &str = "[plugins.skill]\npstack = true\n";
    const PSTACK_ON_DOCS_OFF: &str = "[plugins.skill]\npstack = true\ndocs = false\n";
    const PSTACK_KEY: &str = "plugins.skill.pstack";
    const PLUGINS_TABLE: &str = "plugins";
    const DECISIONS_ADDED: &str = "[agent]\nmax_output_lines = 10\n\n[ui]\ntheme = \"dark\"\n\n[decisions.features]\nmemory_search = \"enforce\"\n";
    const LOADING_PROFILE: (&str, &str) = ("review", "Review carefully.");
    const EMPTY_PROFILE: (&str, &str) = ("blank", "");
    const UNCLOSED_PROFILE: (&str, &str) = ("draft", "---\nlayout: custom\nReview.");
    const EMPTY_FILE: &str = "blank.md";
    const UNCLOSED_FILE: &str = "draft.md";

    fn roots(global: &Path) -> WatchRoots {
        WatchRoots {
            global: Some(global.to_path_buf()),
            home: None,
            project: Vec::new(),
        }
    }

    fn settings_file(dir: &TempDir) -> PathBuf {
        dir.path().join(SETTINGS_FILE)
    }

    #[test]
    fn nothing_to_say_renders_nothing() {
        assert!(Report::default().render().is_none());
    }

    #[test]
    fn problems_lead_then_what_reloaded_then_notes() {
        let report = Report {
            problems: vec![PROBLEM.to_owned()],
            reloaded: vec![FIRST, SECOND],
            notes: vec![NOTE.to_owned()],
        };

        assert_eq!(
            report.render().unwrap(),
            format!(
                "{PROBLEM}{SEPARATOR}{RELOADED} {FIRST}{LIST_SEPARATOR}{SECOND}{SEPARATOR}{NOTE}"
            )
        );
    }

    #[test_case(Change::Edited, true ; "an_edit_is_announced")]
    #[test_case(Change::Own, false ; "caudras_own_write_is_quiet")]
    fn a_merged_part_keeps_its_problems(change: Change, announced: bool) {
        let mut report = Report::default();
        report.merge(
            Report {
                problems: vec![PROBLEM.to_owned()],
                reloaded: vec![FIRST],
                notes: vec![NOTE.to_owned()],
            },
            change,
        );

        assert_eq!(report.problems, [PROBLEM]);
        assert_eq!(!report.reloaded.is_empty(), announced);
        assert_eq!(!report.notes.is_empty(), announced);
    }

    #[test]
    fn a_save_that_changed_nothing_is_not_a_change() {
        let dir = TempDir::new().unwrap();
        fs::write(settings_file(&dir), SETTINGS).unwrap();
        let roots = roots(dir.path());
        let mut prints = Fingerprints::read(&roots);
        fs::write(settings_file(&dir), SETTINGS).unwrap();

        assert!(
            prints
                .triage(&BTreeSet::from([ConfigSource::Settings]), &roots)
                .is_empty()
        );
    }

    #[test_case(false, Change::Edited ; "an_edit_by_someone_else")]
    #[test_case(true, Change::Own ; "a_write_caudra_made")]
    fn a_changed_file_is_triaged_by_who_wrote_it(own: bool, expected: Change) {
        let dir = TempDir::new().unwrap();
        fs::write(settings_file(&dir), SETTINGS).unwrap();
        let roots = roots(dir.path());
        let mut prints = Fingerprints::read(&roots);
        fs::write(settings_file(&dir), AGENT_EDITED).unwrap();
        if own {
            own_writes::record(&settings_file(&dir), AGENT_EDITED.as_bytes());
        }
        let sources = BTreeSet::from([ConfigSource::Settings]);

        assert_eq!(
            prints.triage(&sources, &roots),
            BTreeMap::from([(ConfigSource::Settings, expected)])
        );
        assert!(prints.triage(&sources, &roots).is_empty());
    }

    #[test]
    fn a_directory_source_always_counts_as_edited() {
        let dir = TempDir::new().unwrap();
        let roots = roots(dir.path());
        let mut prints = Fingerprints::read(&roots);

        assert_eq!(
            prints.triage(&BTreeSet::from([ConfigSource::Skills]), &roots),
            BTreeMap::from([(ConfigSource::Skills, Change::Edited)])
        );
    }

    #[test_case(COMMENTED, &[] ; "a_comment")]
    #[test_case(AGENT_EDITED, &[("agent", true)] ; "the_agent_table")]
    #[test_case(UI_AND_FLAG_EDITED, &[("always_yolo", false), ("ui", true)] ; "a_table_and_a_key")]
    #[test_case(SHOW_DECISIONS_EDITED, &[("ui.show_decisions", false)] ; "a_key_that_applies_at_once")]
    #[test_case(UI_AND_SHOW_DECISIONS_EDITED, &[("ui", true), ("ui.show_decisions", false)] ; "that_key_beside_the_rest_of_its_table")]
    #[test_case(DECISIONS_ADDED, &[("decisions", true)] ; "the_decisions_table")]
    fn only_keys_whose_values_changed_count(edited: &str, expected: &[(&str, bool)]) {
        let dir = TempDir::new().unwrap();
        fs::write(settings_file(&dir), SETTINGS).unwrap();
        let roots = roots(dir.path());
        let before = SettingsTables::read(&roots);
        fs::write(settings_file(&dir), edited).unwrap();

        let changed = before.changed_keys(&SettingsTables::read(&roots), true);

        let expected: BTreeMap<String, bool> = expected
            .iter()
            .map(|(key, table)| ((*key).to_owned(), *table))
            .collect();
        assert_eq!(changed, expected);
    }

    #[test_case(PSTACK_OFF, PSTACK_ON, &[(PSTACK_KEY, false)] ; "the_pstack_switch_alone")]
    #[test_case(PSTACK_OFF, PSTACK_ON_DOCS_OFF, &[(PLUGINS_TABLE, true), (PSTACK_KEY, false)] ; "the_switch_beside_another_skill_flag")]
    #[test_case(PSTACK_ON, PSTACK_ON_DOCS_OFF, &[(PLUGINS_TABLE, true)] ; "another_skill_flag_alone")]
    fn a_nested_key_that_applies_at_once_is_named_by_its_path(
        before: &str,
        after: &str,
        expected: &[(&str, bool)],
    ) {
        let dir = TempDir::new().unwrap();
        fs::write(settings_file(&dir), before).unwrap();
        let roots = roots(dir.path());
        let tables = SettingsTables::read(&roots);
        fs::write(settings_file(&dir), after).unwrap();

        let changed = tables.changed_keys(&SettingsTables::read(&roots), true);

        let expected: BTreeMap<String, bool> = expected
            .iter()
            .map(|(key, table)| ((*key).to_owned(), *table))
            .collect();
        assert_eq!(changed, expected);
        assert_eq!(
            settings_note(&changed)
                .is_some_and(|note| note.contains(&format!("{PSTACK_KEY} {NOW_SUFFIX}"))),
            expected.contains_key(PSTACK_KEY)
        );
    }

    #[test]
    fn the_note_says_when_each_changed_key_applies() {
        let changed = BTreeMap::from([
            ("agent".to_owned(), true),
            ("always_yolo".to_owned(), false),
            ("autopilot".to_owned(), true),
            ("experimental".to_owned(), true),
            ("ui".to_owned(), true),
        ]);

        assert_eq!(
            settings_note(&changed).unwrap(),
            format!(
                "[agent]{LIST_SEPARATOR}[autopilot] {LIVE_SUFFIX}{LIST_SEPARATOR}always_yolo{LIST_SEPARATOR}[ui] {RELOAD_SUFFIX}{LIST_SEPARATOR}[experimental] {RESTART_SUFFIX}"
            )
        );
    }

    #[test]
    fn decision_settings_apply_at_once_and_the_rest_of_ui_waits() {
        let changed = BTreeMap::from([
            ("agent".to_owned(), true),
            ("decisions".to_owned(), true),
            ("ui".to_owned(), true),
            ("ui.show_decisions".to_owned(), false),
        ]);

        assert_eq!(
            settings_note(&changed).unwrap(),
            format!(
                "[decisions]{LIST_SEPARATOR}ui.show_decisions {NOW_SUFFIX}{LIST_SEPARATOR}[agent] {LIVE_SUFFIX}{LIST_SEPARATOR}[ui] {RELOAD_SUFFIX}"
            )
        );
    }

    #[test]
    fn websearch_settings_apply_at_once() {
        let changed = BTreeMap::from([
            ("agent".to_owned(), true),
            (WEBSEARCH_TABLE.to_owned(), true),
        ]);

        assert_eq!(
            settings_note(&changed).unwrap(),
            format!("[{WEBSEARCH_TABLE}] {NOW_SUFFIX}{LIST_SEPARATOR}[agent] {LIVE_SUFFIX}")
        );
    }

    #[test]
    fn nothing_changed_needs_no_note() {
        assert!(settings_note(&BTreeMap::new()).is_none());
    }

    fn saved_profiles(saved: &[(&str, &str)]) -> PromptProfileCatalog {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        for (name, text) in saved {
            fs::write(profiles.join(format!("{name}.{PROFILE_EXTENSION}")), text).unwrap();
        }
        PromptProfileCatalog::discover_with(Some(dir.path()), false)
    }

    #[test_case(&[LOADING_PROFILE], &[] ; "every_profile_loads")]
    #[test_case(&[UNCLOSED_PROFILE, LOADING_PROFILE, EMPTY_PROFILE], &[EMPTY_FILE, UNCLOSED_FILE] ; "each_broken_file_is_named")]
    fn the_problem_names_each_profile_that_failed_to_load(saved: &[(&str, &str)], named: &[&str]) {
        let problem = profile_problem(&saved_profiles(saved));

        assert_eq!(problem.is_some(), !named.is_empty());
        let files: Vec<&str> = problem
            .iter()
            .flat_map(|problem| problem.split(LIST_SEPARATOR))
            .filter_map(|part| part.split_once(": ").map(|(file, _)| file))
            .collect();
        assert_eq!(files, named);
    }
}
