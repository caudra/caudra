#![forbid(unsafe_code)]

//! Typed confined or host-managed filesystem tools with an optional MCP adapter.

mod catalog;
mod diff;
mod error;
mod glob;
#[cfg(test)]
mod glob_tests;
mod group;
#[cfg(feature = "index")]
mod index;
mod model_text;
mod mutation_operations;
mod operations;
mod patch;
mod path_policy;
mod prepared;
mod read_operations;
mod text;
mod types;
mod workspace;

#[cfg(feature = "mcp")]
pub use catalog::catalog;
pub use catalog::specs;
pub use error::FilesystemError;
pub use group::FileToolGroup;
#[cfg(all(feature = "index", feature = "mcp"))]
pub use group::fit_index_output;
#[cfg(feature = "mcp")]
pub use group::{fit_glob_output, fit_grep_output};
#[cfg(feature = "index")]
pub use index::*;
pub use model_text::ModelText;
#[cfg(feature = "index")]
pub use prepared::PreparedFileIndex;
pub use prepared::{
    PreparedFileEdit, PreparedFileGlob, PreparedFileGrep, PreparedFilePatch, PreparedFileRead,
    PreparedFileWrite,
};
pub use types::*;
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};
pub use workspace::{
    PreparedWorkspaceMutation, RootResourceKind, WorkspaceError, WorkspaceRepositoryResource,
    WorkspaceResolvedPath, WorkspaceSnapshotAccess, WorkspaceWatchBatch, WorkspaceWatchEvent,
    WorkspaceWatchFailure, WorkspaceWatcher, root_relative_resource_id,
    root_relative_resource_scope,
};
