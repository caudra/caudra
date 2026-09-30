use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use caudra_agent::permissions::{
    PluginRuleStore, VerifiedLocalSourceLocator, canonical_json_sha256,
};
use caudra_agent::tools::ToolRegistry;
use caudra_config::config_file::{global_init_lua_path, project_init_lua_path};
use caudra_config::{AgentConfig, PluginsConfig, RawConfig};
use include_dir::{Dir, include_dir};

use crate::api::keymap::KeymapReader;
use crate::api::options::{PluginOptionSpecs, PluginOpts};
use crate::api::tool::PermissionRulePolicy;
use crate::api::util::command::{HintReader, LuaCommandReader, UiAction};
use crate::error::PluginError;
use crate::plugin_permissions::{PluginPermissions, load_plugin_permissions_with_trust};
use crate::runtime::{self, ClickFallback, LuaThread, Request, RestoreItem};
use caudra_agent::prompt::ResolvedSlots;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a blocked caller looks up to check the runtime still exists.
/// Only a caller whose request died with the host ever waits a whole one: a
/// live runtime's reply wakes the receive the moment it is sent.
const HOST_LIVENESS_POLL: Duration = Duration::from_millis(50);
/// Bundled plugins that load regardless of the user's `plugins` config.
/// Empty since `tool_output` became a native tool; kept because the mechanism
/// is how any future non-negotiable builtin would arrive.
const ALWAYS_LOADED_BUILTINS: &[&str] = &[];

struct BundledPlugin {
    name: &'static str,
    dir: Dir<'static>,
}

fn bundled_implementation_digest() -> String {
    fn collect(plugin: &str, directory: &Dir<'_>, files: &mut BTreeMap<String, String>) {
        for file in directory.files() {
            if file
                .path()
                .extension()
                .is_some_and(|extension| extension == "lua")
            {
                files.insert(
                    format!("{plugin}/{}", file.path().display()),
                    canonical_json_sha256(&serde_json::Value::String(
                        file.contents_utf8().unwrap_or_default().into(),
                    )),
                );
            }
        }
        for child in directory.dirs() {
            collect(plugin, child, files);
        }
    }

    let mut files = BTreeMap::new();
    for plugin in BUNDLED_PLUGINS {
        collect(plugin.name, &plugin.dir, &mut files);
    }
    canonical_json_sha256(&serde_json::json!(files))
}

/// `lib` is not a default builtin; it exists so plugins can
/// `require()` shared modules across boundaries.
static BUNDLED_PLUGINS: &[BundledPlugin] = &[
    BundledPlugin {
        name: "sessions",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/sessions"),
    },
    BundledPlugin {
        name: "index",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/index"),
    },
    BundledPlugin {
        name: "webfetch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    },
    BundledPlugin {
        name: "websearch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    },
    BundledPlugin {
        name: "bash",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    },
    BundledPlugin {
        name: "batch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/batch"),
    },
    BundledPlugin {
        name: "grep",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/grep"),
    },
    BundledPlugin {
        name: "glob",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/glob"),
    },
    BundledPlugin {
        name: "skill",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/skill"),
    },
    BundledPlugin {
        name: "memory",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/memory"),
    },
    BundledPlugin {
        name: "question",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/question"),
    },
    BundledPlugin {
        name: "todo_write",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/todo_write"),
    },
    BundledPlugin {
        name: "tool_output",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/tool_output"),
    },
    BundledPlugin {
        name: "read",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/read"),
    },
    BundledPlugin {
        name: "write",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/write"),
    },
    BundledPlugin {
        name: "edit",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/edit"),
    },
    BundledPlugin {
        name: "task",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/task"),
    },
    BundledPlugin {
        name: "python_execution",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/python_execution"),
    },
    BundledPlugin {
        name: "view_image",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/view_image"),
    },
    BundledPlugin {
        name: "lib",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    },
    BundledPlugin {
        name: "list",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/list"),
    },
];

pub(crate) fn lib_dir() -> &'static Dir<'static> {
    &BUNDLED_PLUGINS
        .iter()
        .find(|p| p.name == "lib")
        .expect("lib plugin bundled")
        .dir
}

static BUNDLED_DIRS: LazyLock<&'static [&'static Dir<'static>]> = LazyLock::new(|| {
    let dirs: Vec<&'static Dir<'static>> = BUNDLED_PLUGINS.iter().map(|p| &p.dir).collect();
    Vec::leak(dirs)
});

pub struct PluginHost {
    /// `None` when Lua is off for the whole process: no VM, thread, or
    /// watchdog exists, executable requests are refused, and everything else
    /// answers inertly.
    lua: Option<LuaThread>,
    plugin_rules: Arc<PluginRuleStore>,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(handle) = self.lua.as_mut().and_then(|lua| lua.join.take()) else {
            return;
        };
        // Start the shutdown first, or the join below waits for all
        // queued bulk work to drain.
        self.begin_shutdown();
        let (done_tx, done_rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

impl PluginHost {
    pub fn new(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        Self::with_jit(registry, true)
    }

    /// `jit: false` (the `--no-jit` flag) runs plugin Lua on the O1
    /// interpreter with full debug info. Applied at VM creation, so
    /// every chunk gets it, init.lua files included.
    pub fn with_jit(registry: Arc<ToolRegistry>, jit: bool) -> Result<Self, PluginError> {
        let plugin_rules = Arc::new(PluginRuleStore::default());
        let lua = runtime::spawn(registry, *BUNDLED_DIRS, jit, Arc::clone(&plugin_rules))?;
        Ok(Self {
            lua: Some(lua),
            plugin_rules,
        })
    }

    /// The host of a process that runs no Lua, because
    /// `experimental.lua_plugins` is off or `--no-plugins` forced it off. The
    /// permission-rule store still serves native rules.
    pub fn disabled() -> Self {
        Self {
            lua: None,
            plugin_rules: Arc::default(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.lua.is_some()
    }

    fn lua(&self) -> Result<&LuaThread, PluginError> {
        self.lua.as_ref().ok_or(PluginError::Disabled)
    }

    /// The store that `caudra.api.register_permission_rule` writes into. Hand
    /// it to every [`caudra_agent::permissions::PermissionManager`] so plugin
    /// rules apply to all sessions.
    pub fn plugin_rules(&self) -> Arc<PluginRuleStore> {
        Arc::clone(&self.plugin_rules)
    }

    /// Stop the Lua thread from taking new work without joining it, so the
    /// caller can rebuild shared state (like the tool registry) while the
    /// old VM winds down on its own. The flag makes the watchdog abort
    /// in-flight callbacks, `Shutdown` on the priority lane skips ahead of
    /// queued bulk work, and swapping the senders for disconnected ones
    /// makes every later host call fail right at the send; `&mut self`
    /// rules out a call racing the swap. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        let Some(lua) = &mut self.lua else {
            return;
        };
        lua.shutdown.store(true, Ordering::Release);
        let _ = lua.prio_tx.send(Request::Shutdown);
        lua.tx = flume::unbounded().0;
        lua.prio_tx = flume::unbounded().0;
    }

    /// A disabled host has nothing to stop; a live one that already lost its
    /// thread reports [`PluginError::HostDead`].
    pub fn shutdown_checked(&mut self) -> Result<(), PluginError> {
        self.shutdown_checked_with_timeout(SHUTDOWN_TIMEOUT)
    }

    fn shutdown_checked_with_timeout(&mut self, timeout: Duration) -> Result<(), PluginError> {
        if self.lua.is_none() {
            return Ok(());
        }
        self.begin_shutdown();
        let handle = self
            .lua
            .as_mut()
            .and_then(|lua| lua.join.take())
            .ok_or(PluginError::HostDead)?;
        let (done_tx, done_rx) = flume::bounded(1);
        thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(timeout) {
            Ok(false) => Ok(()),
            Ok(true) | Err(flume::RecvTimeoutError::Disconnected) => {
                Err(PluginError::ShutdownPanicked)
            }
            Err(flume::RecvTimeoutError::Timeout) => Err(PluginError::ShutdownTimeout),
        }
    }

    /// Boots the runtime and loads every default bundled plugin into `registry`.
    /// For callers like tests and docgen that want the full builtin set
    /// without building a config.
    pub fn with_all_builtins(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let mut host = Self::new(registry)?;
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))?;
        Ok(host)
    }

    /// Runs the global `init.lua` in `config_dir`, if there is one. It is
    /// trusted, and its settings rank with the global `caudra.toml`.
    pub fn run_global_init(&self, config_dir: &Path) -> Result<Option<RawConfig>, PluginError> {
        self.run_init_file(
            &global_init_lua_path(config_dir),
            "global/init.lua",
            PermissionRulePolicy::Trusted,
        )
    }

    /// Runs `.caudra/init.lua` under `cwd`, if there is one. It may only add
    /// deny rules, and its settings apply as a project layer.
    pub fn run_project_init(&self, cwd: &Path) -> Result<Option<RawConfig>, PluginError> {
        self.run_init_file(
            &project_init_lua_path(cwd),
            "project/init.lua",
            PermissionRulePolicy::DenyOnly,
        )
    }

    /// Refuses before touching the file, so a disabled host never reads a
    /// script it would not run.
    fn run_init_file(
        &self,
        path: &Path,
        label: &str,
        rule_policy: PermissionRulePolicy,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.lua()?;
        if !path.is_file() {
            return Ok(None);
        }
        let source_path = fs::canonicalize(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let source = fs::read_to_string(&source_path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        let local_source = loaded_entrypoint(&source_path, &source);
        self.send_run_init_lua_with_policy(
            source,
            label.to_owned(),
            plugin_dir,
            rule_policy,
            local_source,
        )
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        self.load_builtins_from(config, None)
    }

    /// Production runs no Lua built-in on a disabled host, so there is
    /// nothing to load rather than anything to refuse.
    pub fn load_production_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        if !self.is_enabled() {
            return Ok(());
        }
        self.load_builtins_from(config, Some(caudra_config::ACTIVE_DEFAULT_LUA_PLUGINS))
    }

    fn load_builtins_from(
        &self,
        config: &PluginsConfig,
        allowlist: Option<&[&str]>,
    ) -> Result<(), PluginError> {
        let lua = self.lua()?;
        let result = self.send_builtin_loads(config, allowlist);
        // Armed even when a load failed, so a caller that only warns about the
        // error is not left interpreting for the rest of the session.
        let _ = lua.tx.send(Request::WarmJit);
        result
    }

    fn send_builtin_loads(
        &self,
        config: &PluginsConfig,
        allowlist: Option<&[&str]>,
    ) -> Result<(), PluginError> {
        for (plugin, opts) in &config.opts {
            if plugin == "index" {
                continue;
            }
            let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
            if !BUNDLED_PLUGINS.iter().any(|p| p.name == plugin.as_str()) {
                return Err(PluginError::UnknownPluginOptions {
                    plugin: plugin.clone(),
                    keys: keys.join(", "),
                });
            }
            if !config.names.contains(plugin) {
                tracing::warn!(
                    plugin = plugin.as_str(),
                    keys = keys.join(", "),
                    "plugin is disabled; its plugins.{} options are ignored until re-enabled",
                    plugin
                );
            }
        }
        let mut builtins: Vec<String> = config
            .names
            .iter()
            .filter(|name| allowlist.is_none_or(|active| active.contains(&name.as_str())))
            .cloned()
            .collect();
        for builtin in ALWAYS_LOADED_BUILTINS {
            if !builtins.iter().any(|name| name == builtin) {
                builtins.push((*builtin).to_owned());
            }
        }
        for builtin in &builtins {
            let dir = match BUNDLED_PLUGINS.iter().find(|p| p.name == builtin.as_str()) {
                Some(p) => &p.dir,
                None => {
                    return Err(PluginError::UnknownPlugin {
                        plugin: builtin.clone(),
                    });
                }
            };
            let init = dir
                .get_file("init.lua")
                .and_then(|f| f.contents_utf8())
                .ok_or_else(|| PluginError::Lua {
                    plugin: builtin.clone(),
                    source: mlua::Error::runtime("bundled plugin missing init.lua"),
                })?;
            let name: Arc<str> = Arc::from(builtin.as_str());
            let opts = config
                .opts
                .get(builtin.as_str())
                .cloned()
                .map(Arc::new)
                .unwrap_or_default();
            let source = format!(
                "{init}\n-- caudra bundled implementation {}",
                bundled_implementation_digest()
            );
            self.send_load(
                name,
                source,
                None,
                true,
                PluginPermissions::trusted(),
                PermissionRulePolicy::Trusted,
                opts,
                None,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn send_load(
        &self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
        bundled: bool,
        permissions: PluginPermissions,
        rule_policy: PermissionRulePolicy,
        opts: PluginOpts,
        local_source: Option<VerifiedLocalSourceLocator>,
    ) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.lua()?
            .tx
            .send(Request::LoadSource {
                name,
                source,
                plugin_dir,
                bundled,
                permissions,
                rule_policy,
                opts,
                local_source,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Option specs declared by loaded plugins via `caudra.api.register_options`,
    /// keyed by plugin name. Used by docgen.
    pub fn plugin_options(&self) -> Result<PluginOptionSpecs, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.lua()?
            .tx
            .send(Request::CollectPluginOptions { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.send_run_init_lua_with_policy(
            source,
            source_name,
            plugin_dir,
            PermissionRulePolicy::DenyOnly,
            None,
        )
    }

    fn send_run_init_lua_with_policy(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
        rule_policy: PermissionRulePolicy,
        local_source: Option<VerifiedLocalSourceLocator>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.lua()?
            .tx
            .send(Request::RunInitLua {
                source,
                source_name,
                plugin_dir,
                rule_policy,
                local_source,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.lua()?
            .tx
            .send(Request::ClearPlugin {
                plugin: Arc::from(plugin),
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?;
        Ok(())
    }

    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.load_source_with_opts(name, source, serde_json::Map::new())
    }

    pub fn load_source_with_opts(
        &self,
        name: &str,
        source: &str,
        opts: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            source.to_owned(),
            None,
            false,
            PluginPermissions::trusted(),
            PermissionRulePolicy::Trusted,
            Arc::new(opts),
            None,
        )
    }

    pub fn load_source_with_permissions(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            source.to_owned(),
            None,
            false,
            permissions,
            PermissionRulePolicy::Trusted,
            PluginOpts::default(),
            None,
        )
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        self.lua()?;
        let source_path = fs::canonicalize(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let source = fs::read_to_string(&source_path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        let (permissions, trusted) = load_plugin_permissions_with_trust(plugin_dir.as_deref());
        let local_source = loaded_entrypoint(&source_path, &source);
        // Test-only path today. Once user plugin dirs exist: derive a real
        // plugin name, since the hardcoded "user" would collide across files,
        // pass the `plugins.<name>` opts through, and teach the
        // unknown-plugin guards about user plugin names.
        self.send_load(
            Arc::from("user"),
            source,
            plugin_dir,
            false,
            permissions,
            if trusted {
                PermissionRulePolicy::Trusted
            } else {
                PermissionRulePolicy::DenyOnly
            },
            PluginOpts::default(),
            local_source,
        )
    }

    pub fn event_handle(&self) -> EventHandle {
        self.lua
            .as_ref()
            .map_or_else(EventHandle::inert, |lua| EventHandle {
                tx: lua.tx.clone(),
                prio_tx: lua.prio_tx.clone(),
            })
    }

    pub fn command_reader(&self) -> LuaCommandReader {
        self.lua
            .as_ref()
            .map_or_else(LuaCommandReader::empty, |lua| lua.command_reader.clone())
    }

    pub fn keymap_reader(&self) -> KeymapReader {
        self.lua
            .as_ref()
            .map_or_else(KeymapReader::empty, |lua| lua.keymap_reader.clone())
    }

    pub fn hint_reader(&self) -> HintReader {
        self.lua
            .as_ref()
            .map_or_else(HintReader::empty, |lua| lua.hint_reader.clone())
    }

    /// Already disconnected on a disabled host, so a consumer's disconnect
    /// check drops it instead of polling a channel nobody writes.
    pub fn ui_action_rx(&self) -> flume::Receiver<UiAction> {
        self.lua
            .as_ref()
            .map_or_else(|| flume::unbounded().1, |lua| lua.ui_action_rx.clone())
    }
}

fn loaded_entrypoint(path: &Path, source: &str) -> Option<VerifiedLocalSourceLocator> {
    match VerifiedLocalSourceLocator::from_loaded_entrypoint(path, source.as_bytes()) {
        Ok(locator) => Some(locator),
        Err(error) => {
            tracing::warn!(error = %error, "loaded plugin entrypoint is not currently navigable");
            None
        }
    }
}

#[derive(Clone)]
pub struct EventHandle {
    tx: flume::Sender<Request>,
    /// User-initiated requests bypass queued bulk work (session restores).
    prio_tx: flume::Sender<Request>,
}

impl EventHandle {
    pub(crate) fn from_tx(tx: flume::Sender<Request>) -> Self {
        Self {
            tx,
            prio_tx: flume::unbounded().0,
        }
    }

    /// A handle no runtime drains, for a process without Lua: every request
    /// settles at once with its default, so nothing ever waits on one.
    pub fn inert() -> Self {
        Self::from_tx(flume::unbounded().0)
    }

    #[doc(hidden)]
    pub fn disconnected_for_test() -> Self {
        Self::inert()
    }

    /// True when no runtime is draining requests. Production handles stay
    /// connected for the host's lifetime; the disconnected-for-test handle
    /// and a host whose thread has shut down both report true. Callers use
    /// this to skip async side effects (e.g. a restore-complete flip) that
    /// no live consumer would ever observe.
    pub fn is_disconnected(&self) -> bool {
        self.tx.is_disconnected() && self.prio_tx.is_disconnected()
    }

    /// Test probe sibling of `from_tx`: collapses both senders onto one
    /// channel so a `RequestProbe` sees every request, including the
    /// `prio_tx`-routed commands and keybind callbacks that `from_tx`
    /// would route to a disconnected channel.
    pub(crate) fn probed_for_test(shared: flume::Sender<Request>) -> Self {
        Self {
            tx: shared.clone(),
            prio_tx: shared,
        }
    }

    pub fn run_command(&self, plugin: Arc<str>, command: Arc<str>, args: String, depth: u8) {
        let _ = self.prio_tx.try_send(Request::RunCommand {
            plugin,
            command,
            args,
            depth,
        });
    }

    /// Waits for a reply, and settles for the default if the runtime dies
    /// with the request still queued.
    ///
    /// A dying runtime drains what it holds and then spends the teardown of a
    /// whole Lua VM still owning its receivers, so a request sent in that
    /// window is accepted by a channel nobody will read again. Dropping the
    /// receivers does not free it: flume keeps a queued message alive while
    /// any sender exists, and this handle is one. The reply sender inside it
    /// therefore never drops, and a plain `recv` would park here for good.
    /// The receivers going is what says the host is gone, and only a waiter
    /// can see it happen, so the waiter is where it has to be checked.
    fn await_reply<T: Default>(&self, rx: &flume::Receiver<T>) -> T {
        loop {
            match rx.recv_timeout(HOST_LIVENESS_POLL) {
                Ok(reply) => return reply,
                Err(flume::RecvTimeoutError::Disconnected) => return T::default(),
                Err(flume::RecvTimeoutError::Timeout) if self.tx.is_disconnected() => {
                    return T::default();
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// [`Self::await_reply`] for a caller that must not block its executor.
    async fn await_reply_async<T: Default>(&self, rx: &flume::Receiver<T>) -> T {
        loop {
            let reply = async { Some(rx.recv_async().await) };
            let lull = async {
                smol::Timer::after(HOST_LIVENESS_POLL).await;
                None
            };
            match smol::future::or(reply, lull).await {
                Some(Ok(reply)) => return reply,
                Some(Err(_)) => return T::default(),
                None if self.tx.is_disconnected() => return T::default(),
                None => {}
            }
        }
    }

    pub fn collect_prompt_slots(&self, config: &AgentConfig) -> ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots {
            config: config.clone(),
            reply: tx,
        });
        self.await_reply(&rx)
    }

    pub async fn collect_prompt_slots_async(&self, config: &AgentConfig) -> ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots {
            config: config.clone(),
            reply: tx,
        });
        self.await_reply_async(&rx).await
    }

    pub fn request_restore(&self, item: RestoreItem, event_tx: caudra_agent::EventSender) {
        let _ = self.tx.send(Request::RestoreToolAsync {
            item: Box::new(item),
            event_tx,
        });
    }

    /// `row` is the 1-based line in the tool's live buffer, 0 for clicks
    /// outside it (header line etc.).
    pub fn request_click(&self, tool_use_id: String, row: usize) {
        let _ = self.tx.send(Request::ClickTool {
            tool_use_id,
            row,
            fallback: None,
        });
    }

    /// Like [`Self::request_click`], but when the runtime no longer holds
    /// a live or warm handle for the tool it restores from `item` (whose
    /// `clicks` must already include `row`) and emits fresh snapshots on
    /// `event_tx`. Callers need no knowledge of the runtime's warm cache.
    pub fn request_click_with_fallback(
        &self,
        tool_use_id: String,
        row: usize,
        item: RestoreItem,
        event_tx: caudra_agent::EventSender,
    ) {
        let _ = self.tx.send(Request::ClickTool {
            tool_use_id,
            row,
            fallback: Some(Box::new(ClickFallback { item, event_tx })),
        });
    }

    /// Clears `flag` once every restore queued before it has landed. With no
    /// runtime to deliver the barrier to there is nothing to wait for, so it
    /// clears at once rather than leaving the caller restoring forever.
    pub fn send_restore_complete(&self, flag: Arc<AtomicBool>) {
        if let Err(flume::SendError(Request::RestoreComplete { flag })) =
            self.tx.send(Request::RestoreComplete { flag })
        {
            flag.store(false, Ordering::Relaxed);
        }
    }

    /// Blocks until every restore item queued so far has finished; restores
    /// run as spawned tasks, and the `RestoreComplete` flag flips only once
    /// the whole batch has landed, making it the batch barrier.
    #[doc(hidden)]
    pub fn wait_restore_complete_for_test(&self) {
        const DEADLINE: Duration = Duration::from_secs(30);
        let flag = Arc::new(AtomicBool::new(true));
        self.send_restore_complete(Arc::clone(&flag));
        let start = std::time::Instant::now();
        while flag.load(Ordering::Relaxed) {
            assert!(start.elapsed() < DEADLINE, "restore batch never completed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn fire_autocmd(&self, event: &str, data: serde_json::Value) {
        let _ = self.tx.try_send(Request::FireAutocmd {
            event: event.to_owned(),
            data,
        });
    }

    pub fn run_keybind_callback(&self, id: u64) -> bool {
        self.prio_tx
            .try_send(Request::RunKeybindCallback { id })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{LuaCommandInfo, LuaCommandWriter};
    use caudra_agent::permissions::PermissionManager;
    use caudra_agent::prompt::{PromptId, ResolvedSlots, Slot};
    use caudra_agent::tools::ToolRegistry;
    use caudra_config::decisions::DecisionsConfigError;
    use caudra_config::{ConfigError, FeatureMode, PermissionsConfig, ToolKey};
    use std::time::Instant;
    use test_case::test_case;
    use url::Url;

    const REMOVED_ENDPOINT_MESSAGE: &str = "unknown field `endpoint`, expected one of `base_url`";

    #[test_case(false; "plugin_file")]
    #[test_case(true; "init_file")]
    fn permission_provenance_tracks_loaded_entrypoint_not_its_label(init: bool) {
        const RULE: &str = r#"caudra.api.register_permission_rule({ tool = "provenance_tool", scope = "*", effect = "deny" })"#;
        const LABEL: &str = "not-a-filesystem-path";
        const TOOL: &str = "provenance_tool";
        const CHANGED: &str = "invalid lua content";
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("init.lua");
        fs::write(&path, RULE).unwrap();
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let manager = PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            temp.path().to_path_buf(),
            host.plugin_rules(),
        );
        if init {
            host.run_init_file(&path, LABEL, PermissionRulePolicy::DenyOnly)
                .unwrap();
        } else {
            host.load_plugin_file(&path).unwrap();
        }
        let entry = manager
            .active_policy()
            .into_iter()
            .find(|entry| entry.rule.tool == ToolKey::native(TOOL))
            .unwrap();
        let locator = entry.verified_local_source_locator.unwrap();
        assert_eq!(locator.path(), path.canonicalize().unwrap());
        assert!(locator.is_plugin_entrypoint());
        locator.verify_current().unwrap();
        fs::write(&path, CHANGED).unwrap();
        assert!(locator.verify_current().is_err());
        host.load_source(if init { LABEL } else { "user" }, RULE)
            .unwrap();
        let entry = manager
            .active_policy()
            .into_iter()
            .find(|entry| entry.rule.tool == ToolKey::native(TOOL))
            .unwrap();
        assert!(entry.verified_local_source_locator.is_none());
    }

    /// jit=true is exercised by the whole integration suite
    /// (`tests/plugin_host.rs` boots hosts via `new`); only the O1
    /// interpreter path needs its own coverage.
    #[test]
    fn with_jit_off_loads_builtins_and_registers_tools() {
        let reg = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::with_jit(Arc::clone(&reg), false).unwrap();
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .unwrap();
        assert!(matches!(
            reg.get("glob").unwrap().source,
            caudra_agent::tools::ToolSource::Lua { bundled: true, .. }
        ));
    }

    /// Plugin ids that live in-tree as Lua reference implementations but are
    /// owned natively in production. Loading one would shadow the native tool,
    /// fail registration outright on the name conflict, or — where the native
    /// name has since changed under them — quietly offer the same capability
    /// twice.
    const NATIVELY_OWNED: &[&str] = &[
        "bash",
        "batch",
        "python_execution",
        "edit",
        "glob",
        "grep",
        "index",
        "list",
        "memory",
        "question",
        "read",
        "skill",
        "task",
        "todo_write",
        "tool_output",
        "view_image",
        "webfetch",
        "websearch",
        "write",
    ];

    #[test]
    fn production_builtins_leave_natively_owned_tools_alone() {
        let reg = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_production_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .unwrap();

        for name in NATIVELY_OWNED {
            assert!(reg.get(name).is_none(), "{name} was registered from Lua");
            assert!(!caudra_config::ACTIVE_DEFAULT_LUA_PLUGINS.contains(name));
        }
        assert!(caudra_config::WORKCELL_NATIVE_TOOL_NAMES.contains(&"file_index"));
        assert!(caudra_config::CAUDRA_NATIVE_TOOL_NAMES.contains(&"view_image"));
    }

    #[test]
    fn dynamically_loaded_tools_are_not_marked_as_bundled() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source(
            "replacement",
            r#"
            caudra.api.register_tool({
                name = "task",
                description = "replacement",
                schema = { type = "object", properties = {} },
                handler = function() return "ok" end,
            })
            "#,
        )
        .unwrap();
        assert!(matches!(
            reg.get("task").unwrap().source,
            caudra_agent::tools::ToolSource::Lua { bundled: false, .. }
        ));
    }

    #[test]
    fn trusted_init_policy_requires_a_valid_plugin_manifest() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let directory = tempfile::TempDir::new().unwrap();
        let result = host.send_run_init_lua_with_policy(
            r#"caudra.api.register_permission_rule({ tool = "bash", scope = "*", effect = "allow" })"#
                .into(),
            "global/init.lua".into(),
            Some(directory.path().to_path_buf()),
            PermissionRulePolicy::Trusted,
            None,
        );

        assert!(result.is_err());
        assert!(host.plugin_rules().snapshot().is_empty());
    }

    /// The second call sends `Shutdown` on a sender that is already
    /// disconnected; it must swallow that error and keep rejecting work.
    #[test]
    fn begin_shutdown_rejects_later_loads_and_is_idempotent() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.begin_shutdown();
        assert!(host.load_source("late", "return {}").is_err());
        host.begin_shutdown();
        assert!(host.load_source("later", "return {}").is_err());
    }

    #[test]
    fn shutdown_checked_stops_vm_and_rejects_later_loads() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let events = host.event_handle();
        host.shutdown_checked().unwrap();
        let lua = host.lua.as_ref().unwrap();
        assert!(lua.shutdown.load(Ordering::Acquire));
        assert!(lua.join.is_none());
        assert!(events.is_disconnected());
        assert!(matches!(
            host.load_source("late", "return {}"),
            Err(PluginError::HostDead)
        ));
        assert!(matches!(
            host.shutdown_checked(),
            Err(PluginError::HostDead)
        ));
    }

    #[test_case(false; "stopped")]
    #[test_case(true; "panicked")]
    fn shutdown_checked_reports_join_result(panics: bool) {
        const PANIC_MESSAGE: &str = "gated vm panic";
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.shutdown_checked().unwrap();
        let (release_tx, release_rx) = flume::bounded(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        host.lua.as_mut().unwrap().join = Some(thread::spawn(move || {
            release_rx.recv().unwrap();
            assert!(!panics, "{PANIC_MESSAGE}");
            thread_stopped.store(true, Ordering::Release);
        }));
        release_tx.send(()).unwrap();

        let result = host.shutdown_checked();
        if panics {
            assert!(matches!(result, Err(PluginError::ShutdownPanicked)));
        } else {
            result.unwrap();
            assert!(stopped.load(Ordering::Acquire));
        }
        assert!(matches!(
            host.shutdown_checked(),
            Err(PluginError::HostDead)
        ));
    }

    #[test]
    fn shutdown_checked_timeout_never_reports_detached_vm_as_stopped() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.shutdown_checked().unwrap();
        let (release_tx, release_rx) = flume::bounded(1);
        let (stopped_tx, stopped_rx) = flume::bounded(1);
        host.lua.as_mut().unwrap().join = Some(thread::spawn(move || {
            if release_rx.recv().is_ok() {
                let _ = stopped_tx.send(());
            }
        }));

        let result = host.shutdown_checked_with_timeout(Duration::ZERO);
        assert!(matches!(result, Err(PluginError::ShutdownTimeout)));
        assert!(matches!(
            host.shutdown_checked(),
            Err(PluginError::HostDead)
        ));
        assert!(stopped_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        stopped_rx.recv_timeout(SHUTDOWN_TIMEOUT).unwrap();
    }

    fn hinted_host() -> (PluginHost, EventHandle) {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "hinted",
            r#"caudra.api.register_prompt_hint({ slot = "tool_usage", content = "live" })"#,
        )
        .unwrap();
        let handle = host.event_handle();
        host.begin_shutdown();
        (host, handle)
    }

    /// An `EventHandle` clone keeps a sender alive, which keeps queued
    /// requests alive, so a request that lands after the exiting runtime's
    /// drain is never freed and its reply sender never drops. The two calls
    /// straddle the window deliberately: the first races the teardown, the
    /// second is long past it.
    #[test]
    fn live_event_handle_does_not_hang_after_begin_shutdown() {
        let (host, handle) = hinted_host();

        let slots = handle.collect_prompt_slots(&AgentConfig::default());
        assert!(
            contents(&slots, PromptId::System, Slot::ToolUsage).is_empty(),
            "dead host must yield defaults, not real slots"
        );

        drop(host);
        let slots = handle.collect_prompt_slots(&AgentConfig::default());
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    /// The agent loop assembles its prompt through the async twin, so it has
    /// the same hazard and needs the same answer.
    #[test]
    fn live_event_handle_does_not_hang_asynchronously_either() {
        let (host, handle) = hinted_host();

        let slots = smol::block_on(handle.collect_prompt_slots_async(&AgentConfig::default()));
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());

        drop(host);
        let slots = smol::block_on(handle.collect_prompt_slots_async(&AgentConfig::default()));
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    /// Load `src` as one plugin, collect resolved slots.
    /// Panics on failure; use `load_err` to inspect errors.
    fn slots_from(plugin: &str, src: &str) -> (PluginHost, ResolvedSlots) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(plugin, src).unwrap();
        let slots = host
            .event_handle()
            .collect_prompt_slots(&AgentConfig::default());
        (host, slots)
    }

    fn contents(slots: &ResolvedSlots, prompt: PromptId, slot: Slot) -> Vec<&str> {
        slots
            .get(prompt, slot)
            .iter()
            .map(|e| e.content.as_str())
            .collect()
    }

    #[test]
    fn command_writer_reader_pair_works() {
        let (writer, reader) = LuaCommandWriter::new();
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 0);

        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/test"),
            description: Arc::from("desc"),
            plugin: Arc::from("p"),
            max_args: 0,
        }]);
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 1);
        assert!(snap.generation > 0);
    }

    #[test]
    fn memory_builtin_registers_command() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
        let reader = host.command_reader();
        let snap = reader.load();
        let found = snap.commands.iter().any(|c| c.name.as_ref() == "/memory");
        assert!(
            found,
            "Expected /memory command, found: {:?}",
            snap.commands
                .iter()
                .map(|c| c.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn run_command_sends_correct_request() {
        let (prio_tx, prio_rx) = flume::bounded(8);
        let (tx, _rx) = flume::bounded(8);
        let handle = EventHandle { tx, prio_tx };
        handle.run_command(
            Arc::from("myplugin"),
            Arc::from("/greet"),
            "world".into(),
            2,
        );
        let req = prio_rx.try_recv().unwrap();
        match req {
            Request::RunCommand {
                plugin,
                command,
                args,
                depth,
            } => {
                assert_eq!(plugin.as_ref(), "myplugin");
                assert_eq!(command.as_ref(), "/greet");
                assert_eq!(args, "world");
                assert_eq!(depth, 2);
            }
            _ => panic!("expected RunCommand"),
        }
    }

    #[test]
    fn multiple_plugins_register_independent_commands() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source(
            "plugin_a",
            r#"
            caudra.api.register_command({
                name = "/alpha",
                description = "from a",
                handler = function() end,
            })
            "#,
        )
        .unwrap();
        host.load_source(
            "plugin_b",
            r#"
            caudra.api.register_command({
                name = "/beta",
                description = "from b",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 2);
        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        assert!(names.contains(&"/alpha"));
        assert!(names.contains(&"/beta"));
    }

    #[test]
    fn register_command_adds_missing_leading_slash() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "noslash",
            r#"
            caudra.api.register_command({
                name = "hello",
                description = "no slash",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 1);
        assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    }

    #[test]
    fn command_reader_generation_increments_on_publish() {
        let (writer, reader) = LuaCommandWriter::new();
        assert_eq!(reader.load().generation, 0);
        writer.publish(vec![]);
        assert!(reader.load().generation > 0);
    }

    /// End-to-end: a plugin registers a keymap override, the override is published
    /// to the snapshot, EventHandle::run_keybind_callback dispatches the request,
    /// the runtime resolves the Function by id from the registry, and the callback
    /// executes with an observable side effect. This is the load-bearing path the
    /// dispatch reorder and the dead-host fallback rest on; unit tests only cover
    /// the layers in isolation.
    #[test]
    fn keybind_callback_runs_end_to_end() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "kb",
            r#"
            caudra.keymap.set("n", "<C-g>", function()
                caudra.api.register_command({
                    name = "/fired",
                    description = "callback ran",
                    handler = function() end,
                })
            end, { desc = "test override" })
            "#,
        )
        .unwrap();

        let snap = host.keymap_reader().load();
        assert_eq!(snap.entries.len(), 1, "override published to snapshot");
        let entry = &snap.entries[0];
        assert_eq!(entry.desc, "test override");
        assert!(
            host.command_reader().load().commands.is_empty(),
            "callback has not fired yet"
        );

        let handle = host.event_handle();
        handle.run_keybind_callback(entry.id);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let cmds = &host.command_reader().load().commands;
            if cmds.iter().any(|c| c.name.as_ref() == "/fired") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "keybind callback did not register /fired within 2s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Both halves, so the refusal cannot hide a loader that never runs
    /// anything: a live host surfaces the broken script's error, a disabled
    /// one refuses without evaluating it.
    #[test]
    fn disabled_host_refuses_init_files_a_live_host_runs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".caudra")).unwrap();
        fs::write(
            dir.path().join(".caudra/init.lua"),
            "error('broken init lua must not run')",
        )
        .unwrap();
        fs::write(
            dir.path().join("init.lua"),
            "error('broken init lua must not run')",
        )
        .unwrap();

        let disabled = PluginHost::disabled();
        assert!(!disabled.is_enabled());
        assert!(matches!(
            disabled.run_project_init(dir.path()),
            Err(PluginError::Disabled)
        ));
        assert!(matches!(
            disabled.run_global_init(dir.path()),
            Err(PluginError::Disabled)
        ));
        assert!(matches!(
            disabled.load_plugin_file(&dir.path().join("init.lua")),
            Err(PluginError::Disabled)
        ));

        let live = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        assert!(matches!(
            live.run_project_init(dir.path()),
            Err(PluginError::Lua { .. })
        ));
    }

    #[test]
    fn disabled_host_answers_inertly_and_shuts_down_cleanly() {
        let mut host = PluginHost::disabled();
        let handle = host.event_handle();
        assert!(handle.is_disconnected());
        let slots = handle.collect_prompt_slots(&AgentConfig::default());
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
        assert!(!handle.run_keybind_callback(1));
        let restoring = Arc::new(AtomicBool::new(true));
        handle.send_restore_complete(Arc::clone(&restoring));
        assert!(!restoring.load(Ordering::Relaxed));
        assert!(host.ui_action_rx().is_disconnected());
        assert!(host.command_reader().load().commands.is_empty());
        let plugins = PluginsConfig::from_plugins(HashMap::new());
        assert!(host.load_production_builtins(&plugins).is_ok());
        assert!(matches!(
            host.load_builtins(&plugins),
            Err(PluginError::Disabled)
        ));
        host.begin_shutdown();
        assert!(host.shutdown_checked().is_ok());
    }

    #[test]
    fn live_host_without_its_thread_reports_host_dead() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.shutdown_checked().unwrap();
        assert!(matches!(
            host.shutdown_checked(),
            Err(PluginError::HostDead)
        ));
    }

    /// Layers one script the way config loading does: a trusted script
    /// overlays with global authority, a project one only restricts.
    fn run_layer(
        host: &PluginHost,
        path: &Path,
        label: &str,
        rule_policy: PermissionRulePolicy,
        merged: &mut Option<RawConfig>,
    ) -> Result<(), PluginError> {
        if let Some(raw) = host.run_init_file(path, label, rule_policy)? {
            let base = merged.get_or_insert_with(RawConfig::default);
            match rule_policy {
                PermissionRulePolicy::Trusted => base.merge_global(raw),
                PermissionRulePolicy::DenyOnly => base.merge(raw),
            }
        }
        Ok(())
    }

    #[test_case("base_url = 'https://project.example.test'", "base_url")]
    #[test_case("allow_remote = true", "allow_remote")]
    #[test_case("allow_http = true", "allow_http")]
    #[test_case("allow_http = false", "allow_http")]
    #[test_case("api_key_env = 'PROJECT_KEY'", "api_key_env")]
    #[test_case("log = true", "log")]
    #[test_case("log_retention_days = 999", "log_retention_days")]
    #[test_case("thresholds = { auto_flag = 1.0 }", "thresholds")]
    #[test_case(
        "features = { permission_advice = 'shadow' }",
        "features.permission_advice"
    )]
    #[test_case("features = { auto_screening = 'shadow' }", "features.auto_screening")]
    fn decision_setup_rejects_project_escalation(source: &str, field: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        fs::write(
            &path,
            format!("caudra.setup({{ decisions = {{ {source} }} }})"),
        )
        .unwrap();
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        for mut merged in [None, Some(RawConfig::default())] {
            run_layer(
                &host,
                &path,
                "project/init.lua",
                PermissionRulePolicy::DenyOnly,
                &mut merged,
            )
            .unwrap();
            assert!(
                matches!(merged.unwrap().into_config(false), Err(ConfigError::Decisions(DecisionsConfigError::ProjectOverride(actual))) if actual == field)
            );
        }
    }

    #[test]
    fn decision_setup_global_enablement_and_project_restrictions() {
        const GLOBAL: &str = "caudra.setup({ decisions = { base_url = 'http://100.64.0.3:8000/typesafe', allow_remote = true, allow_http = true, log = true, features = { permission_advice = 'advise', auto_screening = 'enforce' } } })";
        const PROJECT: &str = "caudra.setup({ decisions = { log = false, log_retention_days = 7, features = { permission_advice = 'off' } } })";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let mut merged = None;
        fs::write(&path, GLOBAL).unwrap();
        run_layer(
            &host,
            &path,
            "global/init.lua",
            PermissionRulePolicy::Trusted,
            &mut merged,
        )
        .unwrap();
        fs::write(&path, PROJECT).unwrap();
        run_layer(
            &host,
            &path,
            "project/init.lua",
            PermissionRulePolicy::DenyOnly,
            &mut merged,
        )
        .unwrap();
        let config = merged.unwrap().into_config(false).unwrap().decisions;
        assert!(config.allow_remote);
        assert!(config.allow_http);
        assert_eq!(
            config.endpoint().as_ref().map(Url::as_str),
            Some("http://100.64.0.3:8000/typesafe/v1/systemone")
        );
        assert_eq!(config.features.permission_advice, FeatureMode::Off);
        assert_eq!(config.features.auto_screening, FeatureMode::Enforce);
        assert!(!config.log);
        assert_eq!(config.log_retention_days, 7);
    }

    #[test]
    fn decision_setup_rejects_the_removed_endpoint_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        fs::write(
            &path,
            "caudra.setup({ decisions = { endpoint = 'http://127.0.0.1:8000/v1/systemone' } })",
        )
        .unwrap();
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let error = host
            .run_init_file(&path, "global/init.lua", PermissionRulePolicy::Trusted)
            .unwrap_err();
        assert!(
            error.to_string().contains(REMOVED_ENDPOINT_MESSAGE),
            "{error}"
        );
    }

    #[test_case("always_auto", true)]
    #[test_case("always_auto", false)]
    #[test_case("always_yolo", true)]
    #[test_case("always_yolo", false)]
    fn permission_mode_setup_is_global_only(field: &str, value: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        fs::write(&path, format!("caudra.setup({{ {field} = {value} }})")).unwrap();
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        for mut merged in [None, Some(RawConfig::default())] {
            run_layer(
                &host,
                &path,
                "project/init.lua",
                PermissionRulePolicy::DenyOnly,
                &mut merged,
            )
            .unwrap();
            assert!(
                matches!(merged.unwrap().into_config(false), Err(ConfigError::ProjectPermissionMode(actual)) if actual == field)
            );
        }
        let mut global = None;
        run_layer(
            &host,
            &path,
            "global/init.lua",
            PermissionRulePolicy::Trusted,
            &mut global,
        )
        .unwrap();
        let config = global.unwrap().into_config(false).unwrap();
        assert_eq!(
            if field == "always_auto" {
                config.always_auto
            } else {
                config.always_yolo
            },
            value
        );
    }

    #[test_case("auto_screening"; "auto_screening_disabled")]
    #[test_case("content_screening"; "content_screening_disabled")]
    fn decision_setup_disabling_screening_preserves_required_auto_prompts(feature: &str) {
        const GLOBAL: &str = "caudra.setup({ always_auto = true, decisions = { features = { auto_screening = 'enforce', content_screening = 'advise' } } })";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let mut merged = None;
        fs::write(&path, GLOBAL).unwrap();
        run_layer(
            &host,
            &path,
            "global/init.lua",
            PermissionRulePolicy::Trusted,
            &mut merged,
        )
        .unwrap();
        fs::write(
            &path,
            format!("caudra.setup({{ decisions = {{ features = {{ {feature} = 'off' }} }} }})"),
        )
        .unwrap();
        run_layer(
            &host,
            &path,
            "project/init.lua",
            PermissionRulePolicy::DenyOnly,
            &mut merged,
        )
        .unwrap();
        let config = merged.unwrap().into_config(false).unwrap();
        assert!(config.always_auto);
        assert!(config.decisions.auto_screening_restricted);
    }

    #[test]
    fn callback_string_lands_in_targeted_prompt_only() {
        let (_host, slots) = slots_from(
            "cb",
            r#"
            caudra.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = function() return "from_cb" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["from_cb"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn callback_returning_nil_contributes_nothing() {
        let (_host, slots) = slots_from(
            "nil_cb",
            r#"
            caudra.api.register_prompt_hint({
                slot = "tool_usage",
                content = function() return nil end,
            })
            "#,
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn callback_receives_effective_agent_config() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "config_cb",
            r#"
            caudra.api.register_prompt_hint({
                slot = "tool_usage",
                content = function(config)
                    return config.no_rtk and "disabled" or "enabled"
                end,
            })
            "#,
        )
        .unwrap();

        let mut config = AgentConfig::default();
        let slots = host.event_handle().collect_prompt_slots(&config);
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["enabled"]
        );

        config.no_rtk = true;
        let slots = host.event_handle().collect_prompt_slots(&config);
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["disabled"]
        );
    }

    /// A hint with no `prompt` is a default: it lands on every prompt that has the slot.
    #[test]
    fn static_no_prompt_lands_on_all_prompts_with_slot() {
        let (_host, slots) = slots_from(
            "static_hint",
            r#"
            caudra.api.register_prompt_hint({
                slot = "efficient_tools",
                content = "index",
            })
            "#,
        );
        for &pid in PromptId::ALL {
            assert_eq!(contents(&slots, pid, Slot::EfficientTools), ["index"]);
        }
    }

    /// `conventions` lives on system and general but not research, so a default
    /// hint follows the slot and skips research.
    #[test]
    fn default_hint_skips_prompts_lacking_the_slot() {
        let (_host, slots) = slots_from(
            "conv",
            r#"
            caudra.api.register_prompt_hint({
                slot = "conventions",
                content = "follow conventions",
            })
            "#,
        );
        for pid in [PromptId::System, PromptId::General] {
            assert_eq!(
                contents(&slots, pid, Slot::Conventions),
                ["follow conventions"]
            );
        }
        assert!(contents(&slots, PromptId::Research, Slot::Conventions).is_empty());
    }

    /// Targeting a prompt that does not have the slot quietly drops the hint.
    #[test]
    fn register_prompt_hint_rejects_incompatible_slot_prompt() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "drop",
            r#"
            caudra.api.register_prompt_hint({
                slot = "after_instructions",
                prompt = "research",
                content = "never lands",
            })
            "#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn prompt_list_targets_each_listed_prompt() {
        const CONTENT: &str = "shared";
        let (_host, slots) = slots_from(
            "list",
            r#"
            caudra.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = { "system", "research" },
                content = "shared",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            [CONTENT]
        );
        assert_eq!(
            contents(&slots, PromptId::Research, Slot::ToolUsage),
            [CONTENT]
        );
        assert!(contents(&slots, PromptId::General, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn multiple_plugins_sorted_by_plugin_name() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        for plugin in ["zzz", "aaa"] {
            host.load_source(
                plugin,
                r#"
                caudra.api.register_prompt_hint({ slot = "tool_usage", content = "from_PLUGIN" })
                "#
                .replace("PLUGIN", plugin)
                .as_str(),
            )
            .unwrap();
        }
        let slots = host
            .event_handle()
            .collect_prompt_slots(&AgentConfig::default());
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["from_aaa", "from_zzz"],
            "entries must be ordered by plugin name"
        );
    }

    /// One plugin can register several hints; unloading it clears all of them.
    #[test]
    fn unload_clears_all_hints_from_plugin() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "multi",
            r#"
            caudra.api.register_prompt_hint({ slot = "tool_usage", prompt = "system", content = "usage" })
            caudra.api.register_prompt_hint({ slot = "conventions", prompt = "system", content = "conv" })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();

        let slots = handle.collect_prompt_slots(&AgentConfig::default());
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["usage"]
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Conventions),
            ["conv"]
        );

        host.unload("multi").unwrap();
        let slots = handle.collect_prompt_slots(&AgentConfig::default());
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
        assert!(contents(&slots, PromptId::System, Slot::Conventions).is_empty());
    }

    #[test_case(r#"{ slot = "nonexistent", content = "x" }"# ; "invalid_slot")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = "nope" }"# ; "invalid_prompt")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = { "system", "bogus" } }"# ; "invalid_prompt_in_list")]
    #[test_case(r#"{ slot = "tool_usage" }"# ; "missing_content")]
    #[test_case(r#"{ content = "x" }"# ; "missing_slot")]
    #[test_case(r#"{ slot = "tool_usage", content = 42 }"# ; "content_wrong_type")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = 42 }"# ; "prompt_wrong_type")]
    fn invalid_hint_spec_is_rejected(spec: &str) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let src = format!("caudra.api.register_prompt_hint({spec})");
        assert!(host.load_source("bad", &src).is_err());
    }

    #[test]
    fn identity_slot_lands_on_system_only() {
        let (_host, slots) = slots_from(
            "id",
            r#"
            caudra.api.set_prompt({
                slot = "identity",
                content = "Custom identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Custom identity"]
        );
        assert!(contents(&slots, PromptId::Research, Slot::Identity).is_empty());
        assert!(contents(&slots, PromptId::General, Slot::Identity).is_empty());
    }

    #[test]
    fn tone_slot_lands_on_system_only() {
        let (_host, slots) = slots_from(
            "tone",
            r#"
            caudra.api.set_prompt({
                slot = "tone",
                content = "Custom tone",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Tone),
            ["Custom tone"]
        );
        assert!(contents(&slots, PromptId::Research, Slot::Tone).is_empty());
        assert!(contents(&slots, PromptId::General, Slot::Tone).is_empty());
    }

    #[test]
    fn singleton_last_wins_across_plugins() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "aaa",
            r#"caudra.api.set_prompt({ slot = "identity", content = "AAA" })"#,
        )
        .unwrap();
        host.load_source(
            "zzz",
            r#"caudra.api.set_prompt({ slot = "identity", content = "ZZZ" })"#,
        )
        .unwrap();
        let slots = host
            .event_handle()
            .collect_prompt_slots(&AgentConfig::default());
        let entries = slots.get(PromptId::System, Slot::Identity);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.last().unwrap().content, "ZZZ");
    }

    #[test]
    fn content_required() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source("bad", r#"caudra.api.set_prompt({ slot = "identity" })"#);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("'content' is required"));
    }

    #[test]
    fn set_prompt_sets_identity() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            caudra.api.set_prompt({
                slot = "identity",
                content = "New identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["New identity"]
        );
    }

    #[test]
    fn set_prompt_explicit_system_prompt() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            caudra.api.set_prompt({
                slot = "identity",
                prompt = "system",
                content = "Explicit identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Explicit identity"]
        );
    }

    #[test]
    fn prompt_field_targets_specific_prompt() {
        let (_host, slots) = slots_from(
            "targeter",
            r#"
            caudra.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = "General hint",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["General hint"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn set_prompt_invalid_prompt_rejected() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"caudra.api.set_prompt({ slot = "identity", prompt = "nope", content = "x" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_and_register_prompt_hint_coexist() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "hint",
            r#"caudra.api.register_prompt_hint({ slot = "tool_usage", content = "HINT" })"#,
        )
        .unwrap();
        host.load_source(
            "setter",
            r#"caudra.api.set_prompt({ slot = "identity", content = "SET" })"#,
        )
        .unwrap();
        let slots = host
            .event_handle()
            .collect_prompt_slots(&AgentConfig::default());
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["HINT"]
        );
        assert_eq!(contents(&slots, PromptId::System, Slot::Identity), ["SET"]);
    }

    #[test]
    fn set_prompt_rejects_aggregate_slot() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"caudra.api.set_prompt({ slot = "tool_usage", content = "nope" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_rejects_incompatible_slot_prompt() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"caudra.api.set_prompt({ slot = "identity", prompt = "research", content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn empty_prompt_table_is_rejected() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"caudra.api.set_prompt({ slot = "identity", prompt = {}, content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("no sequence entries"));
    }

    #[test]
    fn content_must_not_be_empty() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"caudra.api.set_prompt({ slot = "identity", content = "" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn set_prompt_with_callback() {
        let (_host, slots) = slots_from(
            "setter_cb",
            r#"
            caudra.api.set_prompt({
                slot = "identity",
                content = function() return "Dyn identity" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Dyn identity"]
        );
    }
}
