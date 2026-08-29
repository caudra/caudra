//! Protocol-neutral shell execution with an MCP adapter.
//!
//! The crate exposes typed prepare/execute APIs and, with the `mcp` feature, MCP catalog and
//! dispatch integration. Initial working-directory validation is not a sandbox: commands inherit
//! the server process's authority.

#![forbid(unsafe_code)]

// Process and output internals stay private so all callers retain lifecycle cleanup and bounds.
mod catalog;
mod group;
mod output;
mod permission;
mod process;
mod progress;
mod types;
mod workdir;

#[cfg(feature = "mcp")]
pub use catalog::catalog;
pub use catalog::specs;
pub use group::{ShellBuildError, ShellToolGroup};
pub use permission::{
    ShellPermissionPolicy, ShellPermissionPolicyError, ShellPermissionPolicySummary,
};
// These limits are public because hosts may need to describe the same contract outside MCP.
pub use progress::ShellProgressSink;
pub use types::{
    DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, PreparedShell, ShellCommandAnalysis, ShellCommandScope,
    ShellExecution, ShellInput, ShellOutput, ShellProgressChunk, ShellStream,
};
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};
