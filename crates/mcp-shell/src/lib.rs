//! MCP facade for Workcell's local shell executor.
//!
//! This crate owns the wire contract and process lifecycle, while exposing only the tool catalog
//! and dispatch group to the application. The executor is intentionally marked unsafe: validating
//! an initial working directory is not a sandbox, and commands inherit the server's authority.

#![forbid(unsafe_code)]

// Keep implementation details private so callers cannot bypass dispatch validation or lifecycle
// cleanup by composing lower-level process and output helpers themselves.
mod catalog;
mod group;
mod output;
mod permission;
mod process;
mod progress;
mod types;
mod workdir;

pub use catalog::catalog;
pub use group::{ShellBuildError, ShellToolGroup};
pub use permission::{
    ShellPermissionPolicy, ShellPermissionPolicyError, ShellPermissionPolicySummary,
};
// These limits are public because hosts may need to describe the same contract outside MCP.
pub use types::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS};
