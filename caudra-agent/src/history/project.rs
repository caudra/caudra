//! Which sessions count as one project's history, shared by the `history` module and the UI.

use std::path::{Path, PathBuf};

use caudra_storage::checkout;
use caudra_storage::history_search::{ProjectScope, WorkspaceIdentity};
use caudra_storage::paths::canonicalize_clean;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::WorkspaceSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryProject {
    /// What prompts, rules and result rows name the project by.
    pub label: String,
    pub scope: ProjectScope,
}

impl HistoryProject {
    /// A local project: its main checkout and every verified linked worktree, named by the main
    /// checkout. Outside a repository, the directory alone. Reads the filesystem.
    pub fn local(cwd: &Path) -> Self {
        let Some(found) = checkout::discover(cwd) else {
            let root = canonicalize_clean(cwd);
            return Self {
                label: root.display().to_string(),
                scope: ProjectScope::Local(vec![root]),
            };
        };
        let mut roots: Vec<PathBuf> = checkout::members(&found.repository)
            .into_iter()
            .filter(|member| !member.prunable)
            .map(|member| member.root)
            .collect();
        if !roots.contains(&found.root) {
            roots.push(found.root);
        }
        Self {
            label: found.main_root.display().to_string(),
            scope: ProjectScope::Local(roots),
        }
    }

    /// The project a session works in: its remote workspace's, or the local one `cwd` is in.
    pub fn of_session(cwd: &Path, workspace: Option<&WorkspaceSession>) -> Self {
        workspace
            .and_then(remote_identity)
            .map_or_else(|| Self::local(cwd), Self::workspace)
    }

    /// A remote workspace's project, every directory of it.
    pub fn workspace(identity: WorkspaceIdentity) -> Self {
        Self {
            label: identity.project.clone(),
            scope: ProjectScope::Workspace(identity),
        }
    }
}

/// The identity of the remote workspace a session works in, or `None` on this machine.
pub fn remote_identity(workspace: &WorkspaceSession) -> Option<WorkspaceIdentity> {
    StoredWorkspaceBinding::new_with_cursor(
        workspace.binding().clone(),
        workspace.cursor().clone(),
        None,
    )
    .ok()
    .filter(|binding| !binding.is_local())
    .map(|binding| WorkspaceIdentity::from(&binding))
}
