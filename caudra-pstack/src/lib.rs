//! The bundled pstack: poteto's skills, served as pages of one `pstack` skill, and Rust ports of
//! the scripts they run.
//!
//! `upstream/` is the verbatim copy `make sync-pstack` writes. `translated/` replaces each file
//! that names Cursor's tools, fields, paths, or features with its Caudra translation, so a served
//! page needs no mapping (`.caudra/skills/pstack-translate`).

#![forbid(unsafe_code)]

pub mod host;
pub mod library;
pub mod models;
pub mod scripts;

pub use host::{Cancelled, Host, Output};
pub use library::{Agent, Library, NAME, SKILL_DESCRIPTION, Skill, library};
pub use models::{ROLES_FILE, Roles};
