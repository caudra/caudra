//! Caps and spend for a run nobody steers turn by turn, such as the learner
//! or a side question beside the conversation, read from the events it sends.

use std::collections::BTreeSet;

use caudra_providers::TokenUsage;
use caudra_storage::usage_ledger::{LedgerPurpose, TurnUsage, UsageLedger};
use thiserror::Error;
use tracing::warn;

use crate::memory::compactor::Ledger;
use crate::types::{AgentEvent, EventSender, TurnCompleteEvent};

/// The tool calls, and the tokens, one run may spend before it is stopped,
/// with what it has spent so far.
///
/// The tokens counted are the ones the run processed afresh: every request's
/// output, and the input no cache served, `input + cache_creation` on every
/// provider, of every request after the first. The first request's prompt is
/// what the run was handed, so a cold cache spends none of the budget on it.
/// Cache reads never count.
#[derive(Debug, Clone)]
pub struct RunBudget {
    max_tool_calls: usize,
    max_tokens: u64,
    calls: usize,
    tokens: u64,
    requests: usize,
    /// The `code` calls' host calls seen so far, by call id and index, when
    /// they count as tool calls of their own.
    hosted: Option<BTreeSet<(String, usize)>>,
}

/// The cap a run went past.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Overrun {
    #[error("made too many tool calls")]
    ToolCalls,
    #[error("spent too many tokens")]
    Tokens,
}

impl RunBudget {
    pub const fn new(max_tool_calls: usize, max_tokens: u64) -> Self {
        Self::build(max_tool_calls, max_tokens, None)
    }

    /// A budget that also counts each call a `code` snippet makes on the host
    /// as a tool call.
    pub const fn counting_host_calls(max_tool_calls: usize, max_tokens: u64) -> Self {
        Self::build(max_tool_calls, max_tokens, Some(BTreeSet::new()))
    }

    const fn build(
        max_tool_calls: usize,
        max_tokens: u64,
        hosted: Option<BTreeSet<(String, usize)>>,
    ) -> Self {
        Self {
            max_tool_calls,
            max_tokens,
            calls: 0,
            tokens: 0,
            requests: 0,
            hosted,
        }
    }

    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStart(_) => self.calls += 1,
            AgentEvent::CodeProgress(progress) => {
                if let Some(hosted) = &mut self.hosted
                    && hosted.insert((progress.id.clone(), progress.index))
                {
                    self.calls += 1;
                }
            }
            AgentEvent::TurnComplete(turn) => {
                self.tokens += self.fresh_tokens(turn.usage);
                self.requests += 1;
            }
            _ => {}
        }
    }

    fn fresh_tokens(&self, usage: TokenUsage) -> u64 {
        let input = match self.requests {
            0 => 0,
            _ => u64::from(usage.input) + u64::from(usage.cache_creation),
        };
        input + u64::from(usage.output)
    }

    pub fn overrun(&self) -> Option<Overrun> {
        if self.calls > self.max_tool_calls {
            Some(Overrun::ToolCalls)
        } else if self.tokens > self.max_tokens {
            Some(Overrun::Tokens)
        } else {
            None
        }
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

/// Where a run's spend goes: an event for the runtime, and the ledger when
/// the runtime keeps none of its own.
#[derive(Clone)]
pub struct Spend {
    pub events: EventSender,
    pub ledger: Option<Ledger>,
}

impl Spend {
    /// Files one request's spend under `purpose`, once.
    pub async fn report(&self, turn: &TurnCompleteEvent, purpose: LedgerPurpose) {
        self.events.try_send(AgentEvent::ModelUsage {
            usage: turn.usage,
            cost: turn.cost,
            billing: turn.billing,
            provider: turn.provider.clone(),
            model: turn.model.clone(),
            purpose,
            account: turn.account.clone(),
        });
        let Some(Ledger { state, cwd }) = self.ledger.clone() else {
            return;
        };
        let row = TurnUsage {
            provider: turn.provider.clone(),
            account: turn.account.clone().unwrap_or_default(),
            model: turn.model.clone(),
            cwd,
            purpose,
            input: turn.usage.input,
            output: turn.usage.output,
            cache_creation: turn.usage.cache_creation,
            cache_read: turn.usage.cache_read,
            cost: turn.cost,
            subscription: turn.billing.is_subscription(),
        };
        if let Err(error) = smol::unblock(move || UsageLedger::open(&state)?.record(&row)).await {
            warn!(
                %error,
                model = %turn.model,
                purpose = purpose.storage_name(),
                "spend not recorded in the usage ledger"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_providers::{Billing, Message};
    use test_case::test_case;

    use super::*;
    use crate::code::output::{CodeCall, CodeProgressEvent};
    use crate::tools::ToolEffect;
    use crate::types::ToolStartEvent;

    const MAX_CALLS: usize = 2;
    const MAX_TOKENS: u64 = 100;
    const UNDER_TOKENS: u32 = 40;
    const OVER_TOKENS: u32 = 70;
    const PAST_CAP: u32 = 150;
    const CODE_CALL: &str = "code-1";

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            ..TokenUsage::default()
        }
    }

    fn written(cache_creation: u32) -> TokenUsage {
        TokenUsage {
            cache_creation,
            ..TokenUsage::default()
        }
    }

    fn read(cache_read: u32) -> TokenUsage {
        TokenUsage {
            cache_read,
            ..TokenUsage::default()
        }
    }

    fn turn(usage: TokenUsage) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: String::new(),
            provider: String::new(),
            purpose: LedgerPurpose::Chat,
            interrupted: false,
            cost: None,
            billing: Billing::Api,
            account: None,
            context_size: None,
            context_window: 0,
        }))
    }

    fn tool_start() -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: String::new(),
            tool: Arc::from(""),
            effect: ToolEffect::ReadOnly,
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }))
    }

    fn host_call(index: usize) -> AgentEvent {
        AgentEvent::CodeProgress(Box::new(CodeProgressEvent {
            id: CODE_CALL.to_owned(),
            index,
            call: CodeCall::default(),
        }))
    }

    #[test_case(vec![tool_start(), tool_start()], None ; "at_the_call_cap")]
    #[test_case(vec![tool_start(), tool_start(), tool_start()], Some(Overrun::ToolCalls) ; "past_the_call_cap")]
    #[test_case(vec![turn(usage(UNDER_TOKENS, UNDER_TOKENS))], None ; "under_the_token_cap")]
    #[test_case(vec![turn(usage(PAST_CAP, 0))], None ; "the_first_prompt_is_free")]
    #[test_case(vec![turn(written(PAST_CAP))], None ; "the_first_prompt_written_to_the_cache_is_free")]
    #[test_case(vec![turn(usage(0, PAST_CAP))], Some(Overrun::Tokens) ; "the_first_output_counts")]
    #[test_case(vec![turn(usage(0, 0)), turn(usage(UNDER_TOKENS, 0)), turn(usage(OVER_TOKENS, 0))], Some(Overrun::Tokens) ; "past_the_token_cap_over_turns")]
    #[test_case(vec![turn(usage(0, 0)), turn(written(PAST_CAP))], Some(Overrun::Tokens) ; "a_later_cache_write_counts")]
    #[test_case(vec![turn(usage(0, 0)), turn(read(PAST_CAP))], None ; "a_cache_read_never_counts")]
    fn a_budget_reports_the_cap_a_run_went_past(
        events: Vec<AgentEvent>,
        expected: Option<Overrun>,
    ) {
        let mut budget = RunBudget::new(MAX_CALLS, MAX_TOKENS);
        for event in &events {
            budget.observe(event);
        }
        assert_eq!(budget.overrun(), expected);
    }

    #[test_case(RunBudget::new(MAX_CALLS, MAX_TOKENS), None ; "not_counted")]
    #[test_case(RunBudget::counting_host_calls(MAX_CALLS, MAX_TOKENS), Some(Overrun::ToolCalls) ; "counted_once_each")]
    fn host_calls_of_a_code_call(mut budget: RunBudget, expected: Option<Overrun>) {
        for event in [tool_start(), host_call(0), host_call(0), host_call(1)] {
            budget.observe(&event);
        }
        assert_eq!(budget.overrun(), expected);
    }
}
