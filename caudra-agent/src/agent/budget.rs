//! Caps and spend for a run nobody steers turn by turn, such as the learner
//! or a side question beside the conversation, read from the events it sends.

use caudra_storage::usage_ledger::{LedgerPurpose, TurnUsage, UsageLedger};
use thiserror::Error;
use tracing::warn;

use crate::memory::compactor::Ledger;
use crate::types::{AgentEvent, EventSender, TurnCompleteEvent};

/// The tool calls, and the input and output tokens, one run may spend before
/// it is stopped, with what it has spent so far.
#[derive(Debug, Clone)]
pub struct RunBudget {
    max_tool_calls: usize,
    max_tokens: u64,
    calls: usize,
    tokens: u64,
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
        Self {
            max_tool_calls,
            max_tokens,
            calls: 0,
            tokens: 0,
        }
    }

    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStart(_) => self.calls += 1,
            AgentEvent::TurnComplete(turn) => {
                self.tokens += u64::from(turn.usage.input) + u64::from(turn.usage.output);
            }
            _ => {}
        }
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

    use caudra_providers::{Billing, Message, TokenUsage};
    use test_case::test_case;

    use super::*;
    use crate::tools::ToolEffect;
    use crate::types::ToolStartEvent;

    const MAX_CALLS: usize = 2;
    const MAX_TOKENS: u64 = 100;
    const UNDER_TOKENS: u32 = 40;
    const OVER_TOKENS: u32 = 70;

    fn turn(input: u32, output: u32) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage {
                input,
                output,
                ..TokenUsage::default()
            },
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

    #[test_case(vec![tool_start(), tool_start()], None ; "at_the_call_cap")]
    #[test_case(vec![tool_start(), tool_start(), tool_start()], Some(Overrun::ToolCalls) ; "past_the_call_cap")]
    #[test_case(vec![turn(UNDER_TOKENS, UNDER_TOKENS)], None ; "under_the_token_cap")]
    #[test_case(vec![turn(UNDER_TOKENS, 0), turn(OVER_TOKENS, 0)], Some(Overrun::Tokens) ; "past_the_token_cap_over_turns")]
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
}
