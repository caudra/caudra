//! Herdr, the terminal workspace manager Caudra cooperates with when it runs in
//! one of its panes. Every call goes through the `herdr` CLI: its socket serves
//! one request per connection and needs a named pipe on Windows, while the CLI
//! already speaks both and checks the protocol version first.

mod api;
mod cli;
mod env;
mod skill;

pub use api::{
    AGENT, AgentReport, AgentState, HerdrPane, HerdrWorktree, ListingSource, NOT_LINKED_WORKTREE,
    OpenedWorkspace, OpenedWorktree, PaneMetadata, RESUME_COMMAND, SOURCE, WorktreeListing,
    resume_argv, resume_command_line,
};
pub use cli::{HerdrCli, HerdrError};
pub use env::{HerdrEnv, PANE_ENVIRONMENT, command_on_path};
pub use skill::herdr_skill;
