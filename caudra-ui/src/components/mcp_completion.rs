//! Values an MCP server suggests for what the reader is typing: a prompt's
//! argument, or a variable of a resource template.
//!
//! Each keystroke would otherwise be a round trip, so an ask waits until
//! typing has paused for [`COMPLETION_DEBOUNCE`], and a newer ask replaces it.
//! The request runs off the event loop; a frame only ever polls it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use caudra_agent::McpSnapshotReader;
use caudra_agent::mcp::CompletionTarget;
use caudra_agent::mcp::error::McpError;
use caudra_agent::mcp::resources::Completion;
use futures_lite::future;
use smol::Task;
use tracing::debug;

pub(crate) const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);

/// One question for a server: which values fit `argument`, typed so far as
/// `value`, given the arguments already settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionAsk {
    pub target: CompletionTarget,
    pub argument: String,
    pub value: String,
    pub context: BTreeMap<String, String>,
}

#[derive(Default)]
pub(crate) struct McpCompleter {
    /// The ask waiting out the pause, and when the pause ends.
    waiting: Option<(CompletionAsk, Instant)>,
    running: Option<(CompletionAsk, Task<Result<Completion, McpError>>)>,
    /// The last ask sent, so asking it again changes nothing.
    sent: Option<CompletionAsk>,
}

impl McpCompleter {
    /// Asks once typing has paused. Asking what was already asked is a no-op,
    /// so a caller may ask on every frame.
    pub fn ask(&mut self, ask: CompletionAsk, now: Instant) {
        let current = self
            .waiting
            .as_ref()
            .map(|(ask, _)| ask)
            .or(self.sent.as_ref());
        if current == Some(&ask) {
            return;
        }
        self.running = None;
        self.sent = None;
        self.waiting = Some((ask, now + COMPLETION_DEBOUNCE));
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.waiting.is_some() || self.running.is_some()
    }

    /// Sends the ask whose pause has ended, and hands back the answer to one
    /// that finished. A server that fails to answer offers nothing.
    pub fn poll(
        &mut self,
        reader: &McpSnapshotReader,
        now: Instant,
    ) -> Option<(CompletionAsk, Vec<String>)> {
        if self.waiting.as_ref().is_some_and(|(_, due)| *due <= now)
            && let Some((ask, _)) = self.waiting.take()
        {
            let reader = reader.clone();
            let request = ask.clone();
            let task = smol::spawn(async move {
                reader
                    .complete(
                        &request.target,
                        &request.argument,
                        &request.value,
                        &request.context,
                    )
                    .await
            });
            self.sent = Some(ask.clone());
            self.running = Some((ask, task));
        }
        let (_, task) = self.running.as_mut()?;
        let result = future::block_on(future::poll_once(task))?;
        let (ask, _) = self.running.take()?;
        match result {
            Ok(completion) => Some((ask, completion.values)),
            Err(error) => {
                debug!(?ask.target, argument = ask.argument, %error, "MCP completion failed");
                None
            }
        }
    }

    /// Waits for the running ask, for a test that must not race it.
    #[cfg(test)]
    pub fn settle(
        &mut self,
        reader: &McpSnapshotReader,
        now: Instant,
    ) -> Option<(CompletionAsk, Vec<String>)> {
        let answer = self.poll(reader, now);
        if answer.is_some() {
            return answer;
        }
        let (ask, task) = self.running.take()?;
        future::block_on(task)
            .ok()
            .map(|completion| (ask, completion.values))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use caudra_agent::McpSnapshotReader;
    use caudra_agent::mcp::CompletionTarget;

    use super::{COMPLETION_DEBOUNCE, CompletionAsk, McpCompleter};

    const PROMPT: &str = "server/review";
    const ARGUMENT: &str = "branch";

    fn ask(value: &str) -> CompletionAsk {
        CompletionAsk {
            target: CompletionTarget::Prompt {
                qualified_name: PROMPT.into(),
            },
            argument: ARGUMENT.into(),
            value: value.into(),
            context: BTreeMap::new(),
        }
    }

    /// Nothing is sent until typing has paused for the whole debounce, and a
    /// keystroke inside the pause starts it over.
    #[test]
    fn an_ask_waits_out_the_pause_and_a_newer_one_restarts_it() {
        let reader = McpSnapshotReader::empty();
        let mut completer = McpCompleter::default();
        let start = Instant::now();
        completer.ask(ask("ma"), start);
        let almost = start + COMPLETION_DEBOUNCE - Duration::from_millis(1);
        completer.poll(&reader, almost);
        assert!(completer.running.is_none());

        completer.ask(ask("mai"), almost);
        completer.poll(&reader, start + COMPLETION_DEBOUNCE);
        assert!(completer.running.is_none());

        completer.poll(&reader, almost + COMPLETION_DEBOUNCE);
        assert_eq!(completer.sent, Some(ask("mai")));
    }

    #[test]
    fn asking_again_what_was_sent_sends_nothing() {
        let reader = McpSnapshotReader::empty();
        let mut completer = McpCompleter::default();
        let start = Instant::now();
        completer.ask(ask("main"), start);
        completer.settle(&reader, start + COMPLETION_DEBOUNCE);
        completer.ask(ask("main"), start + COMPLETION_DEBOUNCE);
        assert!(!completer.busy());
    }
}
