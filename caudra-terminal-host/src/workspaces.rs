use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::HostError;

/// Where a host has a checkout: Herdr's workspace or Orca's worktree id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostLocation(pub String);

/// A pane to run a command in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPane {
    /// A shell already running, and the tab holding it when the host names
    /// tabs separately.
    Open { pane: String, tab: Option<String> },
    /// A pane the host opens at `location` once there is a command for it.
    Pending {
        location: HostLocation,
        cwd: PathBuf,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum PaneSite<'a> {
    /// Beside the focused pane of `location`, starting in `cwd`.
    Beside {
        location: &'a HostLocation,
        cwd: &'a Path,
    },
    /// In a workspace of its own, starting in the directory given.
    NewWorkspace(&'a Path),
    /// In a tab of its own beside the one Caudra runs in, starting in the
    /// directory given.
    NewTab(&'a Path),
    /// In a tab of its own in `location`, starting in `cwd`.
    TabIn {
        location: &'a HostLocation,
        cwd: &'a Path,
    },
    /// In a tab of its own beside the one holding `pane`, a pane this host
    /// opened, starting in `cwd`.
    TabBeside { pane: &'a HostPane, cwd: &'a Path },
}

/// A checkout the host opened or focused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub location: HostLocation,
    pub root: PathBuf,
    pub branch: Option<String>,
    pub pane: HostPane,
    /// The host only focused what was open already, so its pane is one the
    /// user has been working in.
    pub already_open: bool,
}

/// Where the host has a checkout of a repository, and its main checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Located {
    pub checkout: Option<HostLocation>,
    pub main: Option<HostLocation>,
}

/// Worktrees and panes as a terminal host manages them. Every path is
/// canonical.
pub trait Workspaces: Debug + Send + Sync {
    fn host(&self) -> &'static str;
    /// Whether the host names a branch it is not given, else Caudra does.
    fn names_branches(&self) -> bool;
    /// Creates a checkout of `main_root`'s repository on `branch` starting at
    /// `base`, and focuses it.
    fn create(
        &self,
        main_root: &Path,
        branch: Option<&str>,
        base: &str,
    ) -> Result<Opened, HostError>;
    /// Opens the checkout at `root`, or focuses where it is open.
    fn show(&self, main_root: &Path, root: &Path) -> Result<Opened, HostError>;
    fn locate(&self, main_root: &Path, root: &Path) -> Result<Located, HostError>;
    fn pane(&self, site: PaneSite<'_>) -> Result<HostPane, HostError>;
    /// Runs `command_line` in `pane`, labelling it when asked to.
    fn run(
        &self,
        pane: &HostPane,
        command_line: &str,
        label: Option<&str>,
    ) -> Result<(), HostError>;
    /// Removes the checkout at `location`, keeping its branch. `false` means
    /// the host does not manage it as a worktree, so git has to.
    fn remove(&self, location: &HostLocation, force: bool) -> Result<bool, HostError>;
    /// [`Self::remove`] for a caller to start on its own, because what it
    /// closes holds the caller.
    fn remove_command(&self, location: &HostLocation, force: bool) -> Command;
    /// Whether removing `location` closes the pane Caudra runs in.
    fn holds_this_pane(&self, location: &HostLocation) -> bool;
}
