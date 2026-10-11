//! Herdr, the terminal workspace manager Caudra cooperates with when it runs in
//! one of its panes. Every call goes through the `herdr` CLI: its socket serves
//! one request per connection and needs a named pipe on Windows, while the CLI
//! already speaks both and checks the protocol version first.

mod api;
mod cli;
mod env;
mod status;
mod workspaces;

use std::ffi::OsString;
use std::sync::Arc;

use crate::host::{StatusChannel, TerminalHost};
use crate::skill::SkillSource;
use crate::workspaces::Workspaces;
use env::{
    HERDR_BIN_PATH, HERDR_ENV, HERDR_PANE_ID, HERDR_SOCKET_PATH, HERDR_TAB_ID, HERDR_WORKSPACE_ID,
};

pub use env::HerdrEnv;

pub(crate) const HOST: &str = "Herdr";
/// What Herdr exports to every process in its pane. Shell commands receive it
/// too, so the model's `herdr` calls see the pane the way any other process in
/// it would.
pub const PANE_ENVIRONMENT: &[&str] = &[
    HERDR_ENV,
    HERDR_PANE_ID,
    HERDR_TAB_ID,
    HERDR_WORKSPACE_ID,
    HERDR_SOCKET_PATH,
    HERDR_BIN_PATH,
];
const SKILL_NAME: &str = "herdr";
const SKILL_FLAG: &str = "--skill";
const SKILL_DESCRIPTION: &str = "Control Herdr, the terminal multiplexer this session runs in: inspect and drive its workspaces, tabs, panes, commands, and the agents running in them. Use only when the user asks for Herdr.";
const SKILL_FALLBACK: &str = "Run `herdr --help` to learn the CLI instead.";

#[derive(Clone, Debug)]
pub struct HerdrHost {
    env: HerdrEnv,
}

impl HerdrHost {
    pub fn detect(get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        HerdrEnv::from_env(get).map(|env| Self { env })
    }
}

impl TerminalHost for HerdrHost {
    fn name(&self) -> &'static str {
        HOST
    }

    fn status_channel(&self) -> Option<StatusChannel> {
        Some(StatusChannel::Blocking(Box::new(
            status::HerdrStatusSink::new(&self.env),
        )))
    }

    fn workspaces(&self) -> Option<Arc<dyn Workspaces>> {
        Some(Arc::new(workspaces::HerdrWorkspaces::new(self.env.clone())))
    }

    fn skill(&self) -> Option<SkillSource> {
        let cli = self.env.cli();
        Some(SkillSource {
            host: HOST,
            name: SKILL_NAME,
            description: SKILL_DESCRIPTION,
            command: Arc::new(move || cli.command(&[OsString::from(SKILL_FLAG)])),
            fallback: SKILL_FALLBACK,
        })
    }

    /// Herdr raises its own toast when an agent it tracks blocks or finishes.
    fn raises_notifications(&self) -> bool {
        true
    }
}
