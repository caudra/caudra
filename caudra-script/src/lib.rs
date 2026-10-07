//! The Rhai sandbox that workflow and automation scripts share: a restricted engine, a header
//! read without running the script, a bridge that serves host calls on the caller's thread while
//! the interpreter runs on its own, and the canonical JSON and SHA-256 digests that key host calls.
//!
//! The `rhai` feature (default) provides the engine and the header reader; without it the crate
//! still offers the bridge and the digests.

#![forbid(unsafe_code)]

mod bridge;
mod digest;
#[cfg(feature = "rhai")]
mod header;
#[cfg(feature = "rhai")]
mod sandbox;

pub use bridge::{BridgeClosed, HostBridge, HostKind, InterpreterError, run_interpreter};
pub use digest::{HEX_DIGEST_LEN, canonical_json, sha256_hex};
#[cfg(feature = "rhai")]
pub use header::{HeaderError, ScalarKind, parse_header};
#[cfg(feature = "rhai")]
pub use sandbox::{SandboxLimits, restricted_engine};
