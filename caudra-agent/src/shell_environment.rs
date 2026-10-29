//! What a command Caudra starts on this machine receives: Workcell's fixed allowlist, the
//! terminal host's pane names, the user's `[agent.shell_env]`, then the session's secrets, so the
//! `shell` tool and every other local command see one environment.

use std::ffi::OsString;

use caudra_config::shell_env::{ShellEnvConfig, shell_env_overlay};
use caudra_terminal_host::PANE_ENVIRONMENT;
use workcell_tool_contract::shell_env::command_environment;

use crate::secrets::SessionSecrets;

/// The terminal host's pane names first, then the user's configuration, then the session's
/// secrets, so a configured value wins over a pane name and a session secret over both.
pub fn local_shell_overlay(
    config: &ShellEnvConfig,
    secrets: Option<&SessionSecrets>,
) -> Vec<(String, OsString)> {
    PANE_ENVIRONMENT
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name.to_owned(), value)))
        .chain(shell_env_overlay(config))
        .chain(secrets.into_iter().flat_map(SessionSecrets::overlay))
        .collect()
}

/// The whole environment of a local command, for one started with a cleared environment.
pub fn local_command_environment(
    config: &ShellEnvConfig,
    secrets: Option<&SessionSecrets>,
) -> Vec<(String, OsString)> {
    command_environment(&local_shell_overlay(config, secrets), |name| {
        std::env::var_os(name)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use tempfile::TempDir;

    use super::*;
    use crate::secrets::{SecretValue, SecretVaults};

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const CONFIGURED: &str = "configured-canary";
    const SESSION: &str = "session-canary";

    #[test]
    fn a_session_secret_wins_over_a_configured_value() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let secrets = SecretVaults::for_session(&state, CaudraId::generate());
        secrets
            .set(NAME, SecretValue::new(SESSION.to_owned()))
            .unwrap();
        let config = ShellEnvConfig {
            set: BTreeMap::from([(NAME.to_owned(), CONFIGURED.to_owned())]),
            ..ShellEnvConfig::default()
        };

        let environment = local_command_environment(&config, Some(&secrets));

        let values: Vec<_> = environment
            .iter()
            .filter(|(name, _)| name == NAME)
            .map(|(_, value)| value.to_str().unwrap())
            .collect();
        assert_eq!(values, [SESSION]);
    }
}
