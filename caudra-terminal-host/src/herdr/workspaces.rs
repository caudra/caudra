use std::path::Path;
use std::process::Command;

use tracing::warn;

use super::HOST;
use super::api::{NOT_LINKED_WORKTREE, OpenedWorkspace, OpenedWorktree};
use super::env::HerdrEnv;
use crate::error::HostError;
use crate::workspaces::{HostLocation, HostPane, Located, Opened, PaneSite, Workspaces};

const PANES_OPEN_FIRST: &str = "Herdr opens every pane before it runs a command";

/// Worktrees as Herdr workspaces: each opens grouped with its repository's,
/// and a pane takes commands by being typed into.
#[derive(Debug)]
pub(super) struct HerdrWorkspaces {
    env: HerdrEnv,
}

impl HerdrWorkspaces {
    pub(super) fn new(env: HerdrEnv) -> Self {
        Self { env }
    }
}

fn not_open() -> HostError {
    HostError::Unsupported(HOST, PANES_OPEN_FIRST.into())
}

fn open_pane(workspace: OpenedWorkspace) -> HostPane {
    HostPane::Open {
        pane: workspace.pane_id,
        tab: Some(workspace.tab_id),
    }
}

fn opened(opened: OpenedWorktree) -> Opened {
    Opened {
        location: HostLocation(opened.workspace.workspace_id.clone()),
        root: opened.worktree.path,
        branch: opened.worktree.branch,
        already_open: opened.workspace.already_open,
        pane: open_pane(opened.workspace),
    }
}

impl Workspaces for HerdrWorkspaces {
    fn host(&self) -> &'static str {
        HOST
    }

    fn names_branches(&self) -> bool {
        true
    }

    fn create(
        &self,
        main_root: &Path,
        branch: Option<&str>,
        base: &str,
    ) -> Result<Opened, HostError> {
        self.env
            .cli()
            .worktree_create(main_root, branch, base)
            .map(opened)
    }

    fn show(&self, main_root: &Path, root: &Path) -> Result<Opened, HostError> {
        self.env.cli().worktree_open(main_root, root).map(opened)
    }

    fn locate(&self, main_root: &Path, root: &Path) -> Result<Located, HostError> {
        let listing = self.env.cli().worktree_list(main_root)?;
        Ok(Located {
            checkout: listing
                .workspace_of(root)
                .map(|workspace| HostLocation(workspace.to_owned())),
            main: listing.source.source_workspace_id.map(HostLocation),
        })
    }

    fn pane(&self, site: PaneSite<'_>) -> Result<HostPane, HostError> {
        let cli = self.env.cli();
        let (workspace, cwd) = match site {
            PaneSite::Beside { location, cwd } => {
                return Ok(HostPane::Open {
                    pane: cli.split_workspace(&location.0, cwd)?,
                    tab: None,
                });
            }
            PaneSite::NewWorkspace(cwd) => return cli.workspace_create(cwd).map(open_pane),
            PaneSite::NewTab(cwd) => (self.env.workspace_id.clone(), cwd),
            PaneSite::TabIn { location, cwd } => (Some(location.0.clone()), cwd),
            PaneSite::TabBeside { pane, cwd } => {
                let HostPane::Open { pane, .. } = pane else {
                    return Err(not_open());
                };
                (Some(cli.pane_workspace(pane)?), cwd)
            }
        };
        cli.tab_create(workspace.as_deref(), cwd)
            .map(|opened| HostPane::Open {
                pane: opened.pane_id,
                tab: Some(opened.tab_id),
            })
    }

    fn run(
        &self,
        pane: &HostPane,
        command_line: &str,
        label: Option<&str>,
    ) -> Result<(), HostError> {
        let HostPane::Open { pane, tab } = pane else {
            return Err(not_open());
        };
        let cli = self.env.cli();
        if let Some(label) = label {
            let renamed = match tab {
                Some(tab) => cli.tab_rename(tab, label),
                None => Ok(()),
            }
            .and_then(|()| cli.pane_rename(pane, label));
            if let Err(error) = renamed {
                warn!(%error, ?tab, pane, "Herdr did not label the tab and pane");
            }
        }
        cli.pane_run(pane, command_line)
    }

    fn remove(&self, location: &HostLocation, force: bool) -> Result<bool, HostError> {
        match self.env.cli().worktree_remove(&location.0, force) {
            Ok(()) => Ok(true),
            Err(error) if error.code() == Some(NOT_LINKED_WORKTREE) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn remove_command(&self, location: &HostLocation, force: bool) -> Command {
        self.env.cli().worktree_remove_command(&location.0, force)
    }

    fn holds_this_pane(&self, location: &HostLocation) -> bool {
        self.env.workspace_id.as_deref() == Some(location.0.as_str())
    }
}
