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
mod mutation_operations;
mod operations;
mod patch;
mod path_policy;
mod read_operations;
mod text;
mod types;

#[cfg(feature = "mcp")]
pub use catalog::catalog;
pub use catalog::specs;
pub use error::FilesystemError;
pub use group::{FileToolGroup, PreparedFilePatch};
#[cfg(feature = "index")]
pub use index::*;
pub use types::*;
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};
