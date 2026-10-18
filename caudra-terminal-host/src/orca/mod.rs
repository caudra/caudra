//! Orca, the desktop terminal for agents. Status reaches it in-band as OSC
//! 9999, which it parses out of the pane's output; worktrees and terminals go
//! through its `orca` CLI.

mod cli;
mod env;
mod request;
mod status;
mod workspaces;

use std::ffi::OsString;
use std::process::Command;
use std::sync::Arc;

use tracing::info;

use crate::host::{StatusChannel, TerminalHost};
use crate::mux::TerminalMux;
use crate::skill::SkillSource;
use crate::workspaces::Workspaces;
use env::{
    ORCA_CLI_BIN_DIR, ORCA_CLI_COMMAND, ORCA_PANE_KEY, ORCA_RELAY_DIR, ORCA_RELAY_NODE_PATH,
    ORCA_RELAY_SOCKET_PATH, ORCA_REMOTE_CLI_BIN_DIR, ORCA_TAB_ID, ORCA_TERMINAL_HANDLE,
    ORCA_USER_DATA_PATH, ORCA_WORKSPACE_ID, ORCA_WORKTREE_ID,
};

pub use env::OrcaEnv;
pub use request::HostRequest;

pub(crate) const HOST: &str = "Orca";
/// The `TERM_PROGRAM` Orca's terminals set (`spawn-environment.ts`).
pub(crate) const TERM_PROGRAM_NAME: &str = "Orca";
/// What Orca exports to every process in its terminal, so the model's `orca`
/// calls address the terminal they run in. Orca's hook and launch tokens and
/// its cloud credentials are left out on purpose.
pub const PANE_ENVIRONMENT: &[&str] = &[
    ORCA_PANE_KEY,
    ORCA_TAB_ID,
    ORCA_WORKTREE_ID,
    ORCA_WORKSPACE_ID,
    ORCA_TERMINAL_HANDLE,
    ORCA_USER_DATA_PATH,
    ORCA_CLI_COMMAND,
    ORCA_CLI_BIN_DIR,
    ORCA_REMOTE_CLI_BIN_DIR,
    ORCA_RELAY_DIR,
    ORCA_RELAY_NODE_PATH,
    ORCA_RELAY_SOCKET_PATH,
];
const SKILL_NAME: &str = "orca";
const SKILL_ARGS: [&str; 3] = ["skills", "get", "orca-cli"];
const SKILL_DESCRIPTION: &str = "Control Orca, the terminal app this session runs in: inspect and drive its worktrees, terminals and the agents running in them. Use only when the user asks for Orca.";
const SKILL_FALLBACK: &str = "Run `orca --help` to learn the CLI instead.";

#[derive(Clone, Debug)]
pub struct OrcaHost {
    env: OrcaEnv,
    mux: TerminalMux,
}

impl OrcaHost {
    pub fn detect(get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let env = OrcaEnv::from_env(&get)?;
        Some(Self {
            env,
            mux: TerminalMux::detect_from(get),
        })
    }
}

impl TerminalHost for OrcaHost {
    fn name(&self) -> &'static str {
        HOST
    }

    /// Zellij swallows OSC 9999 rather than pass it through.
    fn status_channel(&self) -> Option<StatusChannel> {
        if self.mux == TerminalMux::Zellij {
            info!("Zellij swallows Orca's status sequence, so Orca shows no status for this pane");
            return None;
        }
        Some(StatusChannel::InBand(Box::new(status::OrcaStatusEncoder)))
    }

    fn workspaces(&self) -> Option<Arc<dyn Workspaces>> {
        Some(Arc::new(workspaces::OrcaWorkspaces::new(&self.env)))
    }

    fn skill(&self) -> Option<SkillSource> {
        let binary = self.env.binary.clone();
        Some(SkillSource {
            host: HOST,
            name: SKILL_NAME,
            description: SKILL_DESCRIPTION,
            command: Arc::new(move || {
                let mut command = Command::new(&binary);
                command.args(SKILL_ARGS);
                command
            }),
            fallback: SKILL_FALLBACK,
        })
    }

    /// Orca stops reading the status of a remote tab once it parks the hidden
    /// tab (stablyai/orca#15442), so its own notice never comes. A bell still
    /// reaches it and marks the tab unread.
    fn raises_notifications(&self) -> bool {
        false
    }

    /// Orca labels a terminal's tab with its window title until the user
    /// renames it.
    fn names_tabs(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const ZELLIJ: &str = "ZELLIJ";
    const TMUX: &str = "TMUX";

    fn host(extra: &'static str) -> OrcaHost {
        OrcaHost::detect(|name| {
            [ORCA_PANE_KEY, extra]
                .contains(&name)
                .then(|| OsString::from("1"))
        })
        .unwrap()
    }

    #[test_case(ZELLIJ, false ; "zellij_swallows_the_status")]
    #[test_case(TMUX, true ; "tmux_passes_it_through")]
    fn status_reaches_orca_unless_zellij_runs_in_between(mux: &'static str, reported: bool) {
        assert_eq!(host(mux).status_channel().is_some(), reported);
    }

    #[test]
    fn no_secret_is_forwarded() {
        for secret in [
            "ORCA_AGENT_HOOK_TOKEN",
            "ORCA_AGENT_LAUNCH_TOKEN",
            "ORCA_CLOUD_AUTH_TOKEN",
            "ORCA_AGENT_HOOK_PORT",
            "ORCA_AGENT_HOOK_ENDPOINT",
        ] {
            assert!(!PANE_ENVIRONMENT.contains(&secret), "{secret}");
        }
    }
}
