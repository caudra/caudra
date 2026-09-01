#![forbid(unsafe_code)]

//! Bounded declarative filtering of captured command output.
//!
//! A tool that captures a command's output can only forward a bounded window of
//! it to a model. Forwarding the raw tail of that window spends the budget on
//! banners, progress lines, and boilerplate. This crate applies a vendored rule
//! set to the captured text first, so the same budget carries proportionally
//! more signal.
//!
//! Three properties matter for callers:
//!
//! * Rules are matched against a *normalized command scope*, not a raw request
//!   string, so path form and shell quoting cannot select or evade a rule.
//! * Filtering is a rendering step. Callers are expected to retain the raw
//!   capture separately; nothing here is a substitute for it.
//! * A command that exited non-zero never has its output replaced by a success
//!   message, even when a rule declares one.
//!
//! The rule corpus is compiled into the binary. Rule files discovered on disk
//! are deliberately not supported, because a rule read from the tree under
//! inspection would let that tree rewrite what the model sees.

mod compile;
mod pipeline;
mod rule;

pub use compile::{Corpus, Rule, builtin};
pub use pipeline::Filtered;
pub use rule::RuleTest;
