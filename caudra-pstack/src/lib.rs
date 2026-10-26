//! The bundled pstack: poteto's skills, served as pages of one `pstack` skill, and Rust ports of
//! the scripts they run.
//!
//! `upstream/` is the verbatim copy `make sync-pstack` writes. `translated/` replaces each file
//! that names Cursor's tools, fields, paths, or features with its Caudra translation, so a served
//! page needs no mapping (`.caudra/skills/skillpack-translate`).

#![forbid(unsafe_code)]

pub mod host;
pub mod library;
pub mod roles;
pub mod scripts;

pub use caudra_skillpack::Skill;
pub use host::{Cancelled, Host, Output};
pub use library::{Agent, Library, NAME, SKILL_DESCRIPTION, automations, is_principle, library};
pub use roles::{
    AutoReason, Candidate, DEFAULT_EFFORT, Kind, MODEL_ROLES_REF, ModelFacts, ResolvedSeat,
    RoleRow, RoleTable, STORED_ROLES_VERSION, Seat, Source, StoredRoles,
};
