//! The bundled pstack: poteto's skills, served as pages of one `pstack` skill, the adapter that
//! maps their Cursor names to Caudra's, and Rust ports of the scripts they run.
//!
//! The skills are embedded verbatim from `upstream/` (see `upstream/upstream.json` and
//! `make sync-pstack`); everything Caudra adds lives beside them.

#![forbid(unsafe_code)]

pub mod host;
pub mod library;
pub mod models;
pub mod scripts;

pub use host::{Cancelled, Host, Output};
pub use library::{ADAPTER, Agent, Library, NAME, SKILL_DESCRIPTION, Skill, library};
pub use models::{ROLES_FILE, Roles};
