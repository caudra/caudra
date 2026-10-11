//! What a local shell command receives beyond Workcell's fixed allowlist, and how the
//! environment tool describes it to the model by name only.

use std::collections::HashSet;
use std::ffi::OsString;

use caudra_config::sandbox::SandboxName;
use caudra_config::shell_env::{ShellEnvConfig, shell_env_overlay};
use caudra_config::shell_visible_environment;
use caudra_terminal_host::PANE_ENVIRONMENT;
use workcell::host_contract::{
    EnvironmentListResponse, ShellEnvironmentCapability, ShellEnvironmentSelection,
};
use workcell::shell::ShellEnvironment;

use crate::RemoteWorkcellClient;

const UNSET_SUFFIX: &str = " (unset)";
const SET_SUFFIX: &str = " (set)";
const LIST_SEPARATOR: &str = ", ";
const GUEST_LISTING_UNAVAILABLE: &str =
    "this sandbox's Workcell does not list its environment; update its image";
pub(crate) const BUILT_IN_ONLY: &str =
    "built-in only; the user can forward more with /env or [agent.shell_env]";

/// The terminal host's pane names first, then the user's configuration, so a configured value wins.
pub(crate) fn local_shell_environment(config: &ShellEnvConfig) -> ShellEnvironment {
    PANE_ENVIRONMENT
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name.to_owned(), value)))
        .chain(shell_env_overlay(config))
        .collect::<Vec<(String, OsString)>>()
        .into()
}

/// Names and counts, never values: `DOCKER_HOST, AWS_* (3 names), SSH_AUTH_SOCK (unset)`.
pub(crate) fn forwarded_summary(config: &ShellEnvConfig) -> String {
    if config.pass.is_empty() && config.set.is_empty() {
        return BUILT_IN_ONLY.to_owned();
    }
    let visible: HashSet<String> = shell_visible_environment()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    config
        .pass
        .iter()
        .map(|pattern| match pattern.exact() {
            Some(name) if visible.contains(name) => name.to_owned(),
            Some(name) => format!("{name}{UNSET_SUFFIX}"),
            None => {
                let matched = visible.iter().filter(|name| pattern.matches(name)).count();
                format!("{pattern} ({matched} names)")
            }
        })
        .chain(config.set.keys().map(|name| format!("{name}{SET_SUFFIX}")))
        .collect::<Vec<_>>()
        .join(LIST_SEPARATOR)
}

/// Where a session's shell commands run, which decides whose variables `/env` edits.
#[derive(Clone)]
pub enum ShellEnvironmentTarget {
    Local,
    /// A Workcell its operator configures. Caudra forwards nothing to it.
    Remote {
        label: String,
    },
    Sandbox(GuestShellEnvironment),
}

/// A sandbox session's guest variable selection. Values resolve in the guest, so only names and
/// literals ever leave this machine.
#[derive(Clone)]
pub struct GuestShellEnvironment {
    client: RemoteWorkcellClient,
    sandbox: SandboxName,
    profile: Option<SandboxName>,
}

impl GuestShellEnvironment {
    /// Applies `selection` to every later shell call on `client`.
    pub fn attach(
        client: RemoteWorkcellClient,
        sandbox: SandboxName,
        profile: Option<SandboxName>,
        selection: ShellEnvironmentSelection,
    ) -> Self {
        client.set_guest_shell_environment(Some(selection));
        Self {
            client,
            sandbox,
            profile,
        }
    }

    pub fn sandbox(&self) -> &SandboxName {
        &self.sandbox
    }

    /// The saved profile the sandbox launched from, which edits persist to.
    pub fn profile(&self) -> Option<&SandboxName> {
        self.profile.as_ref()
    }

    /// `None` when the guest's Workcell cannot forward its variables.
    pub fn capability(&self) -> Option<ShellEnvironmentCapability> {
        self.client.shell_environment_capability().cloned()
    }

    pub fn selection(&self) -> ShellEnvironmentSelection {
        self.client.guest_shell_environment().unwrap_or_default()
    }

    /// Applies from the next shell call.
    pub fn replace(&self, selection: ShellEnvironmentSelection) {
        self.client.set_guest_shell_environment(Some(selection));
    }

    /// The guest's variables as the current selection treats them, for display only.
    pub async fn list(&self) -> Result<EnvironmentListResponse, String> {
        self.client
            .list_guest_environment(&self.selection())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| GUEST_LISTING_UNAVAILABLE.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use workcell_tool_contract::shell_env::EnvPattern;

    use super::*;

    const MISSING: &str = "CAUDRA_SHELL_ENV_SUMMARY_MISSING_CANARY";
    const SET_NAME: &str = "CAUDRA_SHELL_ENV_SUMMARY_SET";

    #[test]
    fn an_empty_configuration_points_at_the_settings() {
        assert_eq!(forwarded_summary(&ShellEnvConfig::default()), BUILT_IN_ONLY);
    }

    #[test]
    fn the_summary_names_unset_and_set_entries_without_values() {
        let config = ShellEnvConfig {
            pass: vec![EnvPattern::Exact(MISSING.into())],
            set: BTreeMap::from([(SET_NAME.into(), "secret-canary".into())]),
        };
        assert_eq!(
            forwarded_summary(&config),
            format!("{MISSING}{UNSET_SUFFIX}{LIST_SEPARATOR}{SET_NAME}{SET_SUFFIX}")
        );
    }
}
