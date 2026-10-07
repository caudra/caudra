//! Runtime-neutral automations: Rhai scripts whose triggers react to session events, whose
//! conditions are ordinary Rhai, and whose actions go through a small host ABI. Pure: no I/O
//! beyond reading the system time-zone database, and the caller supplies the clock.

#![forbid(unsafe_code)]

pub mod args;
pub mod catalog;
pub mod engine;
pub mod event;
pub mod host;
pub mod limits;
pub mod matcher;
pub mod meta;
pub mod replay;
pub mod request;
pub mod schedule;
pub mod snapshot;
pub mod state;
pub mod untrusted;
pub mod validate;

/// The authoring guide the `skill` tool offers as `caudra-automation-dev`, with
/// its frontmatter. It lives beside the engine so its examples are tested
/// against the ABI they describe.
pub const AUTOMATION_SKILL: &str = include_str!("../skill/SKILL.md");
