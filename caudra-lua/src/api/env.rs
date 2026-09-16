use std::path::PathBuf;

use caudra_lua_macro::{lua_fn, lua_table};
use mlua::Lua;

use crate::plugin_permissions::PluginPermissions;

fn utf8(p: PathBuf) -> Option<String> {
    p.into_os_string().into_string().ok()
}

/// Return the directory where caudra stores runtime state (sessions, auth tokens, etc.).
/// Typically `~/.local/state/caudra`, or `~/.local/state/caudra-debug` in debug builds.
/// `CAUDRA_NAMESPACE` overrides the directory name.
///
/// @return (string?) State directory path, or nil if it cannot be determined.
/// @example
/// local dir = caudra.env.state_dir()
#[lua_fn(guard = Env)]
fn state_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(caudra_storage::paths::state_dir().ok().and_then(utf8))
}

/// Return the directory where caudra looks for user configuration files.
/// Typically `~/.config/caudra`, or `~/.config/caudra-debug` in debug builds.
/// `CAUDRA_NAMESPACE` overrides the directory name.
///
/// @return (string?) Config directory path, or nil if it cannot be determined.
/// @example
/// local dir = caudra.env.config_dir()
#[lua_fn(guard = Env)]
fn config_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(caudra_storage::paths::config_dir().ok().and_then(utf8))
}

/// Return the directory where caudra writes its log files (`caudra.log`).
/// Typically `~/.local/logs/caudra`, or `~/.local/logs/caudra-debug` in debug builds.
/// `CAUDRA_NAMESPACE` overrides the directory name.
///
/// @return (string?) Logs directory path, or nil if it cannot be determined.
/// @example
/// local dir = caudra.env.logs_dir()
#[lua_fn(guard = Env)]
fn logs_dir(_lua: &Lua) -> mlua::Result<Option<String>> {
    Ok(caudra_storage::paths::logs_dir().ok().and_then(utf8))
}

lua_table! {
    /// Paths to caudra's own directories (config, state, logs).
    ///
    /// Use these to locate config files or persistent state without hard-coding paths.
    ///
    /// ```lua
    /// local cfg = caudra.env.config_dir()
    /// ```
    "caudra.env" => pub(crate) fn create_env_table(perms: &PluginPermissions), DOCS [
        state_dir(perms), config_dir(perms), logs_dir(perms),
    ]
}
