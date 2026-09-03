#![forbid(unsafe_code)]

//! Bounded rendering of captured command output.
//!
//! A tool that captures a command's output can only forward a bounded window of
//! it to a model. Forwarding the raw tail of that window spends the budget on
//! banners, progress frames, and boilerplate. This crate reduces the captured
//! text first, so the same budget carries proportionally more signal.
//!
//! Three reductions are available, and they are not interchangeable.
//!
//! [`render_terminal`] and [`RowRenderer`] decode a redraw stream. This is not a
//! judgement about content: a bar that redraws emits a control stream, and the
//! rows a terminal would show from it are the text the writer meant a reader to
//! see. Because it is decoding, it is applied unconditionally by callers rather
//! than selected by a rule.
//!
//! [`Corpus::find`] and [`Rule::apply`] are the declarative corpus, selected by
//! command, and are the right tool when the format is known.
//!
//! [`collapse_progress_lines`] reduces progress frames that arrive one per line,
//! which no rule can cover because the programs that emit them are
//! overwhelmingly ones a corpus cannot anticipate. Its gates are narrow and each
//! exists to protect a specific kind of output; see the module documentation.
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
mod progress;
mod rule;
mod terminal;

pub use compile::{Corpus, Rule, builtin};
pub use pipeline::Filtered;
pub use progress::collapse_progress_lines;
pub use rule::RuleTest;
pub use terminal::{RowRenderer, render_terminal};
