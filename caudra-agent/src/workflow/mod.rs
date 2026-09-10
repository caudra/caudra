//! Workflow support around the `caudra-workflow` engine: discovering scripts,
//! persisting runs for one session, and the runtime that executes them.

pub mod catalog;
mod handle;
mod manager;
mod run;
mod skill;
mod state;
pub mod store;

pub use handle::WorkflowHandle;
pub use manager::{RuntimeDeps, WorkflowRuntime};
pub use skill::workflow_dev_skill;
