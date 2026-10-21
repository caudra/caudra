//! What a command Caudra starts on this machine receives: Workcell's fixed allowlist, the
//! terminal host's pane names, then the user's `[agent.shell_env]`, so the `shell` tool and every
//! other local command see one environment.

use std::ffi::OsString;

use caudra_config::shell_env::{ShellEnvConfig, shell_env_overlay};
use caudra_terminal_host::PANE_ENVIRONMENT;
use workcell_tool_contract::shell_env::command_environment;

/// The terminal host's pane names first, then the user's configuration, so a configured value wins.
pub fn local_shell_overlay(config: &ShellEnvConfig) -> Vec<(String, OsString)> {
    PANE_ENVIRONMENT
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name.to_owned(), value)))
        .chain(shell_env_overlay(config))
        .collect()
}

/// The whole environment of a local command, for one started with a cleared environment.
pub fn local_command_environment(config: &ShellEnvConfig) -> Vec<(String, OsString)> {
    command_environment(&local_shell_overlay(config), |name| std::env::var_os(name))
}
