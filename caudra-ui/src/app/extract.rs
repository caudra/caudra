use std::sync::Arc;

use caudra_agent::CancelToken;
use caudra_agent::agent::requirements::{self, REQUIREMENTS_OUTPUT_TOKENS, RequirementsInput};
use caudra_agent::agent::side_model::{self, SideModel};
use caudra_providers::{AgentError, Model, ModelPurpose, ProviderEvent, Timeouts};
use caudra_storage::usage_ledger::LedgerPurpose;
use flume::Sender;

use crate::agent::ModelSlot;
use crate::components::prompt_progress::PromptProgress;
use crate::components::stream_modal::{StreamDone, StreamEvent, StreamFooter, StreamUsage};

use super::App;

const TITLE: &str = " /extract ";
pub(crate) const NOTHING_TO_EXTRACT: &str = "Nothing to extract: the session has no user turns yet";
const EMPTY_LIST: &str = "_The model found no requirements in this session._";

fn header(input: &RequirementsInput, model: Option<&Model>) -> String {
    let with = model.map_or(String::new(), |model| format!(" with {}", model.spec()));
    format!(
        "Extracting requirements from {} user turns and {} answered questions{with}…",
        input.turns(),
        input.answers(),
    )
}

impl App {
    /// Reads the whole transcript, not just what the next request carries, so
    /// a requirement stated before a compaction still makes the list. Resolves
    /// the extractor from the Chat slot, as compaction does, so both name the
    /// same model whatever route the live turn took.
    pub(crate) fn start_extract(&mut self, timeouts: Timeouts, chat: &ModelSlot) {
        let items = match crate::transcript_session_history(&self.state.session) {
            Ok(items) => items,
            Err(error) => {
                self.flash(format!("Failed to read session history: {error}"));
                return;
            }
        };
        let input = RequirementsInput::from_items(&items);
        if input.is_empty() {
            self.flash(NOTHING_TO_EXTRACT.into());
            return;
        }
        self.end_btw_thread();

        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal
            .open(TITLE, header(&input, None), StreamFooter::Copy, rx, trigger);

        let provider = Arc::clone(&chat.provider);
        let model = chat.model.clone();
        let model_policy = Arc::clone(&self.model_policy);
        smol::spawn(async move {
            let side = side_model::resolve(
                ModelPurpose::Extract,
                &provider,
                &model,
                timeouts,
                &model_policy,
                REQUIREMENTS_OUTPUT_TOKENS,
            )
            .await;
            run_extract(side, input, tx, cancel).await;
        })
        .detach();
    }
}

async fn run_extract(
    side: SideModel,
    input: RequirementsInput,
    tx: Sender<StreamEvent>,
    cancel: CancelToken,
) {
    let _ = tx.send(StreamEvent::Header(header(&input, Some(&side.model))));
    let (event_tx, event_rx) = flume::unbounded();
    let forwarder = smol::spawn({
        let tx = tx.clone();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let forwarded = match event {
                    ProviderEvent::TextDelta { text } => StreamEvent::TextDelta(text),
                    ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    } => StreamEvent::Progress(PromptProgress {
                        processed,
                        total,
                        cache,
                    }),
                    _ => continue,
                };
                if tx.send(forwarded).is_err() {
                    return;
                }
            }
        }
    });
    let result = requirements::extract(
        side.provider.as_ref(),
        &side.model,
        &input,
        Some(&event_tx),
        &cancel,
    )
    .await;
    drop(event_tx);
    forwarder.await;

    match result {
        Ok(outcome) => {
            if outcome.text.is_none() {
                let _ = tx.send(StreamEvent::TextDelta(EMPTY_LIST.into()));
            }
            let _ = tx.send(StreamEvent::Done(StreamDone {
                usage: StreamUsage {
                    cost: side.model.billed_cost(&outcome.usage, false),
                    billing: side.model.billing,
                    usage: outcome.usage,
                    model: side.model.id.clone(),
                    provider: side.model.provider.to_string(),
                    purpose: LedgerPurpose::Extract,
                },
                answer: outcome.text,
            }));
        }
        // The receiver is already gone, which is what cancelled the stream.
        Err(AgentError::Cancelled) => {}
        Err(error) => {
            let _ = tx.send(StreamEvent::Error(error.to_string()));
        }
    }
}
