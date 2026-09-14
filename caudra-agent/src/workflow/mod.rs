//! Workflow support around the `caudra-workflow` engine: discovering scripts,
//! persisting runs for one session, and the runtime that executes them.

pub mod catalog;
mod handle;
mod manager;
mod run;
mod skill;
mod state;
pub mod store;

pub use handle::{WorkflowHandle, WorkflowTransition, WorkspaceRebind};
pub use manager::{RuntimeDeps, WorkflowRuntime};
pub use skill::workflow_dev_skill;

pub async fn prepare_workspace_transition(
    workflow: Option<&WorkflowHandle>,
    active_agents: usize,
    workspace: WorkspaceRebind,
) -> Result<Option<WorkflowTransition>, String> {
    if active_agents != 0 {
        return Err("background agents must be quiescent before changing workspace context".into());
    }
    caudra_workspace::WorkspacePath::new(&workspace.cwd).map_err(|error| error.to_string())?;
    let Some(workflow) = workflow else {
        return Ok(None);
    };
    let transition = workflow
        .suspend()
        .await
        .map_err(|error| error.to_string())?;
    transition
        .rebind(workspace)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(transition))
}
