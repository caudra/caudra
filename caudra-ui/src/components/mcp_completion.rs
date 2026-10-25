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

/// A question answered off the event loop once typing has paused: a newer
/// ask replaces one still waiting or running, so only the latest answers.
pub(crate) struct AsyncAsk<A, V> {
    /// The ask waiting out the pause, and when the pause ends.
    waiting: Option<(A, Instant)>,
    running: Option<(A, Task<Option<V>>)>,
    /// The last ask sent, so asking it again changes nothing.
    sent: Option<A>,
}

impl<A, V> Default for AsyncAsk<A, V> {
    fn default() -> Self {
        Self {
            waiting: None,
            running: None,
            sent: None,
        }
    }
}

impl<A: Clone + PartialEq, V: Send + 'static> AsyncAsk<A, V> {
    /// Asks once typing has paused. Asking what was already asked is a no-op,
    /// so a caller may ask on every frame.
    pub fn ask(&mut self, ask: A, now: Instant) {
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

    /// Starts the ask whose pause has ended with `start`, and hands back the
    /// answer to one that finished. `None` from a task offers nothing.
    pub fn poll(
        &mut self,
        now: Instant,
        start: impl FnOnce(A) -> Task<Option<V>>,
    ) -> Option<(A, V)> {
        if self.waiting.as_ref().is_some_and(|(_, due)| *due <= now)
            && let Some((ask, _)) = self.waiting.take()
        {
            self.sent = Some(ask.clone());
            self.running = Some((ask.clone(), start(ask)));
        }
        let (_, task) = self.running.as_mut()?;
        let result = future::block_on(future::poll_once(task))?;
        let (ask, _) = self.running.take()?;
        result.map(|values| (ask, values))
    }

    /// The ask waiting out its pause, or else the one last sent.
    #[cfg(test)]
    pub fn pending(&self) -> Option<&A> {
        self.waiting
            .as_ref()
            .map(|(ask, _)| ask)
            .or(self.sent.as_ref())
    }

    /// Waits for the running ask, for a test that must not race it.
    #[cfg(test)]
    pub fn settle(
        &mut self,
        now: Instant,
        start: impl FnOnce(A) -> Task<Option<V>>,
    ) -> Option<(A, V)> {
        let answer = self.poll(now, start);
        if answer.is_some() {
            return answer;
        }
        let (ask, task) = self.running.take()?;
        future::block_on(task).map(|values| (ask, values))
    }
}

/// The server's `complete` for `ask`. A server that fails to answer offers
/// nothing.
pub(crate) fn complete(
    reader: &McpSnapshotReader,
    ask: CompletionAsk,
) -> Task<Option<Vec<String>>> {
    let reader = reader.clone();
    smol::spawn(async move {
        let result: Result<Completion, McpError> = reader
            .complete(&ask.target, &ask.argument, &ask.value, &ask.context)
            .await;
        result
            .inspect_err(|error| {
                debug!(?ask.target, argument = ask.argument, %error, "MCP completion failed");
            })
            .ok()
            .map(|completion| completion.values)
    })
}

/// Values an MCP server suggests, asked through an [`AsyncAsk`].
#[derive(Default)]
pub(crate) struct McpCompleter(AsyncAsk<CompletionAsk, Vec<String>>);

impl McpCompleter {
    pub fn ask(&mut self, ask: CompletionAsk, now: Instant) {
        self.0.ask(ask, now);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.0.busy()
    }

    pub fn poll(
        &mut self,
        reader: &McpSnapshotReader,
        now: Instant,
    ) -> Option<(CompletionAsk, Vec<String>)> {
        self.0.poll(now, |ask| complete(reader, ask))
    }

    #[cfg(test)]
    pub fn settle(
        &mut self,
        reader: &McpSnapshotReader,
        now: Instant,
    ) -> Option<(CompletionAsk, Vec<String>)> {
        self.0.settle(now, |ask| complete(reader, ask))
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
        assert!(completer.0.running.is_none());

        completer.ask(ask("mai"), almost);
        completer.poll(&reader, start + COMPLETION_DEBOUNCE);
        assert!(completer.0.running.is_none());

        completer.poll(&reader, almost + COMPLETION_DEBOUNCE);
        assert_eq!(completer.0.sent, Some(ask("mai")));
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
