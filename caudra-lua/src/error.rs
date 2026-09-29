use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("lua error in {plugin}: {source}")]
    Lua {
        plugin: String,
        #[source]
        source: mlua::Error,
    },
    #[error("plugin {plugin} attempted to shadow existing tool '{tool}'")]
    NameConflict { plugin: String, tool: String },
    #[error("io error loading plugin {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "plugins.{plugin} sets options ({keys}), but there is no bundled plugin named \"{plugin}\""
    )]
    UnknownPluginOptions { plugin: String, keys: String },
    #[error("no bundled plugin named \"{plugin}\" (enabled via plugins.{plugin})")]
    UnknownPlugin { plugin: String },
    #[error("plugin host is not running")]
    HostDead,
    #[error("Lua is turned off for this process")]
    Disabled,
    #[error("lua thread did not stop within the shutdown timeout")]
    ShutdownTimeout,
    #[error("lua thread panicked on shutdown")]
    ShutdownPanicked,
}
