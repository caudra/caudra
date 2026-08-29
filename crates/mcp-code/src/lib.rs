//! MCP facade for Workcell's isolated Python code executor.
//!
//! This crate owns the wire contract and the worker session lifecycle, while exposing only the tool
//! catalog and dispatch group to the application. Unlike the shell executor, this tool is isolated:
//! code runs in a separate `monty` worker process with no mounts and no host functions, so it can
//! reach no file, socket, or environment value. That isolation is a property of what the parent
//! refuses to answer, not of anything the sandboxed code is asked to respect.
//!
//! The interpreter is a Python subset, not CPython. The tool description enumerates the divergences
//! because an agent that does not know them wastes turns rediscovering them.

#![forbid(unsafe_code)]

// Keep implementation details private so callers cannot bypass dispatch validation, the suspension
// refusals, or session teardown by composing lower-level helpers themselves.
mod catalog;
mod diagnose;
mod group;
mod render;
mod subset;
mod suspend;
mod types;
mod worker;

pub use catalog::catalog;
pub use group::{CodeConfiguration, CodeToolGroup};
pub use worker::{CodeBuildError, WORKER_FILE_NAME, resolve_worker};
// These limits are public because hosts may need to describe the same contract outside MCP.
pub use types::{DEFAULT_TIMEOUT_MS, MAX_CODE_BYTES, MAX_TIMEOUT_MS};
// The subset the description advertises, exported so neither a host restating the contract nor the
// conformance tests have to retype it. Retyping is how the description came to name three modules
// the interpreter has never resolved.
pub use subset::{SUBSET_MODULES, UNTYPED_BUILTINS, WITHHELD_BUILTINS};
