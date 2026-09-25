#![forbid(unsafe_code)]

//! Typed confined or host-managed filesystem tools with an optional MCP adapter.

#[cfg(unix)]
mod binary;
mod catalog;
mod diff;
mod error;
mod gitignore;
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
mod snapshot_tree;
mod text;
#[cfg(unix)]
mod transfer_inventory;
mod types;
mod workspace;

#[cfg(unix)]
pub use binary::{
    BinaryError, BinaryPublicationContent, PreparedBinaryPublication, VerifiedBinaryFile,
    digest_file,
};
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
pub use snapshot_tree::{
    SnapshotTreeContent, SnapshotTreeEntry, SnapshotTreeError, SnapshotTreeExpected,
    SnapshotTreeFile, SnapshotTreeLimit, SnapshotTreeLimits, SnapshotTreeLink, SnapshotTreeNode,
    SnapshotTreeObserved, SnapshotTreeStamp, SnapshotTreeWalk,
};
pub use types::*;
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};
pub use workspace::{
    PreparedWorkspaceMutation, RootResourceKind, WorkspaceError, WorkspaceRepositoryResource,
    WorkspaceResolvedPath, WorkspaceSnapshotAccess, WorkspaceSnapshotScope, WorkspaceWatchBatch,
    WorkspaceWatchErrorKind, WorkspaceWatchEvent, WorkspaceWatchFailure, WorkspaceWatchPhase,
    WorkspaceWatcher, root_relative_resource_id, root_relative_resource_scope,
};
