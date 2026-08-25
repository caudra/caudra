#![forbid(unsafe_code)]

//! Root-confined filesystem tools with MCP catalog and dispatch integration.

mod catalog;
mod diff;
mod error;
mod glob;
#[cfg(test)]
mod glob_tests;
mod group;
mod mutation_operations;
mod operations;
mod patch;
mod path_policy;
mod read_operations;
mod text;
mod types;

pub use catalog::catalog;
pub use error::FilesystemError;
pub use group::FileToolGroup;
pub use types::*;
