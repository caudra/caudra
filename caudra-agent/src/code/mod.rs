//! The `code` tool's runtime: Python a snippet runs in an isolated interpreter, and the host
//! modules it reaches beyond the language.
//!
//! Everything here is protocol-neutral about the script language: the gateway answers name lookups
//! and calls, and the runner is whatever executes the snippet. A [`module::HostModule`] describes
//! the calls it offers; only the gateway authorizes, records and runs them.

pub mod args;
pub mod decide;
pub mod gateway;
pub mod history;
pub mod module;
pub mod names;
pub mod output;
pub mod runner;

use caudra_providers::{ContentBlock, Message};

use self::runner::{ReplSlot, SharedRepl};
use crate::tools::CODE_TOOL_NAME;

/// One agent's `code` state, so a subagent's variables never mix with its parent's. A module that
/// keeps per-agent state beyond what [`crate::tools::ToolContext`] already carries holds it here.
#[derive(Clone)]
pub struct CodeSession {
    /// Held for the whole of a run, so one agent's runs never interleave.
    repl: SharedRepl,
}

impl CodeSession {
    /// `history` says whether the conversation ran `code` before: if it did, the variables those
    /// runs kept died with the agent that ran them, and the next run says so.
    pub fn new(history: &[Message]) -> Self {
        let ran_before = history.iter().flat_map(|message| &message.content).any(
            |block| matches!(block, ContentBlock::ToolUse { name, .. } if name == CODE_TOOL_NAME),
        );
        Self {
            repl: ReplSlot::shared(ran_before),
        }
    }

    pub fn repl(&self) -> &SharedRepl {
        &self.repl
    }
}

impl Default for CodeSession {
    fn default() -> Self {
        Self::new(&[])
    }
}
