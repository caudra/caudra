use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1::{
    AgentResponse, Error as AcpError, Meta, PromptResponse, RequestId, StopReason,
};
use caudra_agent::{AgentInput, QueueItemId, SteeringQueue};
use caudra_storage::id::CaudraId;
use flume::{Receiver, Sender, WeakSender};
use serde_json::{Map, Value, json};

use crate::errors::session_ended;
use crate::outbound::Outbound;
use crate::translate::{META_NAMESPACE, TurnSpend};

/// How long a cancelled turn may take to report its own end before the
/// prompt is answered without it.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const META_PROMPT_ID: &str = "promptId";
pub(crate) const META_DURATION_MS: &str = "durationMs";
pub(crate) const META_USAGE: &str = "usage";
pub(crate) const META_DELIVERY: &str = "delivery";
pub(crate) const META_STEERING: &str = "steering";
pub(crate) const META_STEERED_INTO: &str = "steeredInto";
pub(crate) const DELIVERY_STEER: &str = "steer";

/// Names one `session/prompt` on everything its turn sends.
pub(crate) struct PromptIdentity {
    pub(crate) id: String,
    started: Instant,
}

impl PromptIdentity {
    pub(crate) fn new(id: String) -> Self {
        Self {
            id,
            started: Instant::now(),
        }
    }

    /// `_meta` for a `session/update` the turn sends.
    pub(crate) fn update_meta(&self) -> Meta {
        Meta::from_iter([(
            META_NAMESPACE.to_owned(),
            json!({ META_PROMPT_ID: self.id }),
        )])
    }

    /// The `_meta.caudra` of the prompt's answer: its id and how long it ran.
    pub(crate) fn answer_meta(&self) -> Map<String, Value> {
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Map::from_iter([
            (META_PROMPT_ID.to_owned(), Value::from(self.id.as_str())),
            (META_DURATION_MS.to_owned(), Value::from(duration_ms)),
        ])
    }
}

/// What a `session/prompt` asks for in its `_meta`: the id the client gave it,
/// and whether it steers a running turn.
pub(crate) struct PromptDirective {
    pub(crate) identity: PromptIdentity,
    pub(crate) steer: bool,
}

impl PromptDirective {
    pub(crate) fn of(meta: Option<&Meta>) -> Self {
        let id = meta
            .and_then(|meta| meta.get(META_PROMPT_ID))
            .and_then(Value::as_str)
            .map_or_else(|| CaudraId::generate().to_string(), str::to_owned);
        let steer = meta
            .and_then(|meta| meta.get(META_NAMESPACE))
            .and_then(|caudra| caudra.get(META_DELIVERY))
            .and_then(Value::as_str)
            == Some(DELIVERY_STEER);
        Self {
            identity: PromptIdentity::new(id),
            steer,
        }
    }
}

/// A prompt that joined a running turn through the agent's steering queue.
pub(crate) struct Steer {
    pub(crate) request: RequestId,
    pub(crate) identity: PromptIdentity,
    pub(crate) item: QueueItemId,
    /// Whether the turn took it in, as the agent reported.
    pub(crate) consumed: bool,
}

impl Steer {
    fn answer(self, outbound: &Outbound, end: &Result<StopReason, AcpError>, turn: &str) {
        let result = end.clone().map(|stop| {
            let mut caudra = self.identity.answer_meta();
            caudra.insert(META_STEERED_INTO.to_owned(), Value::from(turn));
            let meta = Meta::from_iter([(META_NAMESPACE.to_owned(), Value::Object(caudra))]);
            AgentResponse::PromptResponse(PromptResponse::new(stop).meta(meta))
        });
        outbound.respond(self.request, result);
    }
}

/// A prompt no turn took: a steer withdrawn from the queue, or a prompt sent
/// after its session's turn was cancelled. Either starts the next turn.
pub(crate) struct Leftover {
    request: RequestId,
    identity: PromptIdentity,
    input: AgentInput,
}

impl Leftover {
    pub(crate) fn new(request: RequestId, identity: PromptIdentity, input: AgentInput) -> Self {
        Self {
            request,
            identity,
            input,
        }
    }

    pub(crate) fn request(&self) -> &RequestId {
        &self.request
    }

    /// Answers it the way the turn ahead of it ended, since no turn ran it.
    pub(crate) fn answer(self, outbound: &Outbound, end: &Result<StopReason, AcpError>) {
        let result = end.clone().map(|stop| {
            let response = TurnSpend::default().into_response(stop, &self.identity);
            AgentResponse::PromptResponse(response)
        });
        outbound.respond(self.request, result);
    }
}

/// How a turn ended, as its prompt is answered.
pub(crate) enum TurnEnd {
    Stopped { stop: StopReason, spend: TurnSpend },
    Failed(AcpError),
}

impl TurnEnd {
    pub(crate) fn cancelled() -> Self {
        Self::Stopped {
            stop: StopReason::Cancelled,
            spend: TurnSpend::default(),
        }
    }
}

/// The prompt a session's turn owes an answer, and the steers that joined it.
pub(crate) struct OwedPrompt {
    pub(crate) request: RequestId,
    pub(crate) identity: PromptIdentity,
    /// The client cancelled, so the turn ends `cancelled` however it stops.
    pub(crate) cancelled: bool,
    pub(crate) steers: Vec<Steer>,
    /// A prompt sent after the cancel, which starts the next turn once this one ends.
    pub(crate) next: Option<Leftover>,
}

impl OwedPrompt {
    pub(crate) fn new(request: RequestId, identity: PromptIdentity) -> Self {
        Self {
            request,
            identity,
            cancelled: false,
            steers: Vec::new(),
            next: None,
        }
    }

    /// Answers the prompt and every steer its turn took with how the turn
    /// ended, and hands back what starts the next turn: the prompt sent after
    /// a cancel, or the steers a turn that stopped uncancelled never took.
    /// Any other steer leaves the queue and is answered with this turn.
    pub(crate) fn finish(
        self,
        outbound: &Outbound,
        steering: &SteeringQueue,
        end: TurnEnd,
    ) -> Vec<Leftover> {
        self.settle(outbound, steering, end).1
    }

    /// Answers the prompt and everything waiting behind it, starting nothing.
    pub(crate) fn abandon(self, outbound: &Outbound, steering: &SteeringQueue, end: TurnEnd) {
        let (end, leftovers) = self.settle(outbound, steering, end);
        for leftover in leftovers {
            leftover.answer(outbound, &end);
        }
    }

    fn settle(
        self,
        outbound: &Outbound,
        steering: &SteeringQueue,
        end: TurnEnd,
    ) -> (Result<StopReason, AcpError>, Vec<Leftover>) {
        let end = match end {
            TurnEnd::Stopped { stop, spend } => {
                let stop = if self.cancelled {
                    StopReason::Cancelled
                } else {
                    stop
                };
                let response = spend.into_response(stop, &self.identity);
                outbound.respond(self.request, Ok(AgentResponse::PromptResponse(response)));
                Ok(stop)
            }
            TurnEnd::Failed(error) => {
                outbound.respond(self.request, Err(error.clone()));
                Err(error)
            }
        };
        let resumable = matches!(end, Ok(stop) if stop != StopReason::Cancelled);
        let mut leftovers: Vec<Leftover> = self.next.into_iter().collect();
        for steer in self.steers {
            let untaken = (!steer.consumed)
                .then(|| steering.remove(steer.item))
                .flatten();
            match untaken {
                Some(input) if resumable => {
                    leftovers.push(Leftover::new(steer.request, steer.identity, input));
                }
                _ => steer.answer(outbound, &end, &self.identity.id),
            }
        }
        (end, leftovers)
    }
}

/// Starts the next turn with the first prompt the last one never took; the
/// rest steer that turn. `None` when there was none, or the agent is gone.
pub(crate) fn resubmit(
    leftovers: Vec<Leftover>,
    outbound: &Outbound,
    steering: &SteeringQueue,
    input: &WeakSender<AgentInput>,
) -> Option<OwedPrompt> {
    let mut leftovers = leftovers.into_iter();
    let first = leftovers.next()?;
    let steers = leftovers
        .map(|leftover| Steer {
            request: leftover.request,
            identity: leftover.identity,
            item: steering.push(leftover.input),
            consumed: false,
        })
        .collect();
    let owed = OwedPrompt {
        steers,
        ..OwedPrompt::new(first.request, first.identity)
    };
    if input
        .upgrade()
        .is_some_and(|input| input.send(first.input).is_ok())
    {
        return Some(owed);
    }
    owed.abandon(outbound, steering, TurnEnd::Failed(session_ended()));
    None
}

/// Wakes the event pump when the client cancels a turn, so it starts
/// waiting for the turn to drain.
#[derive(Clone)]
pub(crate) struct CancelSignal {
    raise: Sender<()>,
    raised: Receiver<()>,
}

impl Default for CancelSignal {
    fn default() -> Self {
        let (raise, raised) = flume::unbounded();
        Self { raise, raised }
    }
}

impl CancelSignal {
    pub(crate) fn raise(&self) {
        let _ = self.raise.send(());
    }

    pub(crate) async fn raised(&self) {
        let _ = self.raised.recv_async().await;
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const CLIENT_PROMPT_ID: &str = "client-prompt-1";

    #[test_case(json!({META_PROMPT_ID: CLIENT_PROMPT_ID, META_NAMESPACE: {META_DELIVERY: DELIVERY_STEER}}), Some(CLIENT_PROMPT_ID), true ; "client_id_and_steer")]
    #[test_case(json!({META_NAMESPACE: {META_DELIVERY: "queue"}}), None, false ; "other_delivery")]
    #[test_case(json!({META_PROMPT_ID: 7}), None, false ; "non_string_id")]
    fn a_prompt_reads_its_id_and_delivery(meta: Value, id: Option<&str>, steer: bool) {
        let Value::Object(meta) = meta else {
            panic!("meta is an object");
        };
        let directive = PromptDirective::of(Some(&meta));
        assert_eq!(directive.steer, steer);
        match id {
            Some(id) => assert_eq!(directive.identity.id, id),
            None => assert!(directive.identity.id.parse::<CaudraId>().is_ok()),
        }
    }
}
