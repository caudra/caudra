use std::path::Path;
use std::sync::Arc;

use caudra_agent::agent::requirements::{self, REQUIREMENTS_OUTPUT_TOKENS, RequirementsInput};
use caudra_agent::agent::side_model::{self, SideModel};
use caudra_agent::{AgentEvent, CancelToken, CancelTrigger, DoneReason};
use caudra_providers::{AgentError, ModelPurpose, ProviderEvent, Timeouts};
use caudra_storage::id::CaudraId;
use caudra_storage::usage_ledger::LedgerPurpose;
use crossterm::event::{KeyEvent, MouseEvent};
use flume::{Receiver, Sender};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::agent::ModelSlot;
use crate::chat::Chat;
use crate::components::side_transcript::{SideAction, SideFooter, SideTranscript};
use crate::components::tooltip::Tip;
use crate::components::{DisplayMessage, DisplayRole, Overlay};
use crate::repaint::{Cadence, Dirty};

use super::{App, COPIED_SELECTION};

const TITLE: &str = " /extract ";
const CHAT_NAME: &str = "extract";
const STOPPED_NOTE: &str = "Stopped.";
const EVENT_CAPACITY: usize = 64;
/// One request answers an extraction.
const EXTRACT_TURNS: u32 = 1;
pub(crate) const NOTHING_TO_EXTRACT: &str = "Nothing to extract: the session has no user turns yet";
pub(crate) const EMPTY_LIST: &str = "_The model found no requirements in this session._";

fn header(turns: usize, answers: usize, model: Option<&str>) -> String {
    let with = model.map_or(String::new(), |model| format!(" with {model}"));
    format!(
        "Extracting requirements from {turns} user turns and {answers} answered questions{with}…"
    )
}

/// `/extract`'s window and the request that fills it. Dropping it stops the
/// request.
struct Extraction {
    window: SideTranscript,
    turns: usize,
    answers: usize,
    /// The notice heading the window, replaced once the model is known.
    header: String,
    events: Receiver<AgentEvent>,
    stop: Option<CancelTrigger>,
}

impl Extraction {
    fn name_model(&mut self, model: &str) {
        self.window.chat_mut().remove_notice(&self.header);
        self.header = header(self.turns, self.answers, Some(model));
        self.window.push_notice(self.header.clone());
    }

    fn fail(&mut self, message: String) {
        let chat = self.window.chat_mut();
        chat.flush();
        chat.push(DisplayMessage::new(DisplayRole::Error, message));
        self.window.finish_run(None);
    }

    /// Stops the request and keeps what it streamed.
    fn stop(&mut self) {
        self.stop = None;
        let chat = self.window.chat_mut();
        chat.flush();
        chat.cancel_in_progress();
        self.window.push_notice(STOPPED_NOTE);
        self.window.finish_run(None);
    }
}

#[derive(Default)]
pub(crate) struct Extract {
    open: Option<Extraction>,
}

impl Extract {
    pub(crate) fn is_running(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|extraction| extraction.window.is_running())
    }

    pub(crate) fn contains(&self, pos: Position) -> bool {
        self.open
            .as_ref()
            .is_some_and(|extraction| extraction.window.contains(pos))
    }

    pub(crate) fn scroll_at(&mut self, at: Position, delta: i32) {
        if let Some(extraction) = self.open.as_mut() {
            extraction.window.scroll_at(at, delta);
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.open.as_mut().map_or_else(Rect::default, |extraction| {
            extraction.window.view(frame, area)
        })
    }

    #[cfg(test)]
    pub(crate) fn window(&self) -> Option<&SideTranscript> {
        self.open.as_ref().map(|extraction| &extraction.window)
    }
}

impl Overlay for Extract {
    fn is_open(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|extraction| extraction.window.is_open())
    }

    /// Hides the window. The app settles what the request spent on its next
    /// tick, then drops it, which stops the request.
    fn close(&mut self) {
        if let Some(extraction) = self.open.as_mut() {
            extraction.window.close();
        }
    }

    fn cadence(&self) -> Cadence {
        self.open
            .as_ref()
            .map_or(Cadence::IDLE, |extraction| extraction.window.cadence())
    }

    fn tooltip(&self) -> Option<Tip> {
        self.open.as_ref()?.window.tooltip()
    }
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
        let (tx, cancel) = self.open_extract(input.turns(), input.answers());
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

    /// Opens the window over `turns` user turns and `answers` answered
    /// questions, and hands back the channel its request reports on.
    pub(super) fn open_extract(
        &mut self,
        turns: usize,
        answers: usize,
    ) -> (Sender<AgentEvent>, CancelToken) {
        self.close_extract();
        self.close_btw();
        let (tx, events) = flume::bounded(EVENT_CAPACITY);
        let (stop, cancel) = CancelToken::new();
        let chat = Chat::new(
            CHAT_NAME.to_owned(),
            Path::new(&self.state.session.cwd),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        let mut window = SideTranscript::new(TITLE, SideFooter::Copy, chat);
        let header = header(turns, answers, None);
        window.push_notice(header.clone());
        window.begin_run();
        window.open();
        self.extract.open = Some(Extraction {
            window,
            turns,
            answers,
            header,
            events,
            stop: Some(stop),
        });
        (tx, cancel)
    }

    /// Settles what already arrived, then drops the window and its request.
    pub(super) fn close_extract(&mut self) {
        Overlay::close(&mut self.extract);
        let _ = self.tick_extract();
    }

    /// Draws what the request reported and bills what it spent. A window
    /// something closed goes once that is done.
    pub(super) fn tick_extract(&mut self) -> Dirty {
        let Some(mut extraction) = self.extract.open.take() else {
            return Dirty::NO;
        };
        let mut dirty = Dirty::NO;
        while let Ok(event) = extraction.events.try_recv() {
            dirty = Dirty::YES;
            match event {
                AgentEvent::ModelUsage {
                    usage,
                    cost,
                    billing,
                    provider,
                    model,
                    purpose,
                    account,
                } => self.settle_side_spend(
                    &provider,
                    account.as_deref(),
                    &model,
                    purpose,
                    usage,
                    cost,
                    billing,
                ),
                AgentEvent::MessageStarted {
                    id,
                    model,
                    thinking,
                } => {
                    extraction.name_model(&model);
                    extraction.window.handle_event(AgentEvent::MessageStarted {
                        id,
                        model,
                        thinking,
                    });
                }
                AgentEvent::Error { message } => extraction.fail(message),
                AgentEvent::Done { usage, .. } => extraction.window.finish_run(Some(usage)),
                event => {
                    extraction.window.handle_event(event);
                }
            }
        }
        if !extraction.window.is_open() {
            return Dirty::YES;
        }
        dirty |= extraction.window.tick();
        self.extract.open = Some(extraction);
        dirty
    }

    pub(super) fn handle_extract_key(&mut self, key: KeyEvent) {
        let Some(extraction) = self.extract.open.as_mut() else {
            return;
        };
        let action = extraction.window.handle_key(key);
        self.handle_extract_action(action);
    }

    /// Whether the pointer was the window's.
    pub(super) fn handle_extract_mouse(&mut self, event: MouseEvent) -> bool {
        let Some(extraction) = self.extract.open.as_mut() else {
            return false;
        };
        let action = extraction
            .window
            .handle_mouse(event, Path::new(&self.state.session.cwd));
        let ignored = action == SideAction::Ignored;
        self.handle_extract_action(action);
        !ignored
    }

    fn handle_extract_action(&mut self, action: SideAction) {
        match action {
            SideAction::Close => self.close_extract(),
            SideAction::Copy(text) => self.copy_to_clipboard(&text),
            SideAction::CopySelection(text) => self.copy_labelled(&text, COPIED_SELECTION),
            SideAction::Interrupt => {
                if let Some(extraction) = self.extract.open.as_mut() {
                    extraction.stop();
                }
            }
            SideAction::Ignored
            | SideAction::Consumed
            | SideAction::Submit(_)
            | SideAction::Clear => {}
        }
    }
}

/// Streams the list into the window as the model writes it, then reports
/// what it cost and that it is done. A closed window has dropped the
/// receiver, which is what cancelled the request, so nothing is sent then.
async fn run_extract(
    side: SideModel,
    input: RequirementsInput,
    tx: Sender<AgentEvent>,
    cancel: CancelToken,
) {
    let started = AgentEvent::MessageStarted {
        id: CaudraId::generate(),
        model: side.model.spec(),
        thinking: None,
    };
    if tx.send_async(started).await.is_err() {
        return;
    }
    let (event_tx, event_rx) = flume::unbounded();
    let forwarder = smol::spawn({
        let tx = tx.clone();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let forwarded = match event {
                    ProviderEvent::TextDelta { text } => AgentEvent::TextDelta { text },
                    ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    } => AgentEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    },
                    _ => continue,
                };
                if tx.send_async(forwarded).await.is_err() {
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

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(AgentError::Cancelled) => return,
        Err(error) => {
            let _ = tx
                .send_async(AgentEvent::Error {
                    message: error.to_string(),
                })
                .await;
            return;
        }
    };
    let mut settled = Vec::new();
    if outcome.text.is_none() {
        settled.push(AgentEvent::TextDelta {
            text: EMPTY_LIST.into(),
        });
    }
    settled.push(AgentEvent::ModelUsage {
        usage: outcome.usage,
        cost: side.model.billed_cost(&outcome.usage, false),
        billing: side.model.billing,
        provider: side.model.provider.to_string(),
        model: side.model.id.clone(),
        purpose: LedgerPurpose::Extract,
        account: None,
    });
    settled.push(AgentEvent::Done {
        usage: outcome.usage,
        num_turns: EXTRACT_TURNS,
        reason: DoneReason::EndTurn,
    });
    for event in settled {
        if tx.send_async(event).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::provider::Provider;
    use caudra_providers::{Message, TokenUsage};
    use test_case::test_case;

    use super::*;
    use crate::app::tests::{PendingProvider, btw_reply, gated_provider};
    use crate::components::test_model;

    const TURN: &str = "Keep the cache key stable across restarts of the server";
    const LIST: &str = "- the cache key survives a restart";

    fn input() -> RequirementsInput {
        RequirementsInput::from_items(&crate::history_items(&[Message::user(TURN.into())]))
    }

    enum Provided {
        Answer(&'static str),
        Failure,
        Nothing,
    }

    /// Runs one extraction to its end and collects what it reported. A
    /// provider that never answers is only ever left by cancelling.
    fn reported(provided: Provided) -> Vec<AgentEvent> {
        let (gated, script) = gated_provider();
        let (stop, cancel) = CancelToken::new();
        let provider: Arc<dyn Provider> = match provided {
            Provided::Answer(answer) => {
                script.responses.send(btw_reply(answer)).unwrap();
                gated
            }
            Provided::Failure => {
                drop(script.responses);
                gated
            }
            Provided::Nothing => {
                drop(stop);
                Arc::new(PendingProvider)
            }
        };
        let (tx, events) = flume::unbounded();
        let side = SideModel {
            provider,
            model: test_model(),
        };
        smol::block_on(run_extract(side, input(), tx, cancel));
        events.drain().collect()
    }

    fn streamed(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test_case(LIST, LIST ; "list")]
    #[test_case("", EMPTY_LIST ; "empty_list")]
    fn an_extraction_streams_its_list_then_bills_it(answer: &'static str, shown: &str) {
        let events = reported(Provided::Answer(answer));

        assert!(matches!(
            events.first(),
            Some(AgentEvent::MessageStarted { model, .. }) if *model == test_model().spec()
        ));
        assert_eq!(streamed(&events), shown);
        let billed: Vec<(LedgerPurpose, TokenUsage)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelUsage { purpose, usage, .. } => Some((*purpose, *usage)),
                _ => None,
            })
            .collect();
        assert_eq!(
            billed,
            [(LedgerPurpose::Extract, btw_reply(answer).usage)],
            "the spend is reported once, under the extract purpose"
        );
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[test]
    fn a_failed_extraction_reports_the_error_and_no_spend() {
        let events = reported(Provided::Failure);

        assert!(matches!(events.last(), Some(AgentEvent::Error { .. })));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ModelUsage { .. }))
        );
    }

    #[test]
    fn a_cancelled_extraction_reports_nothing_after_it_started() {
        let events = reported(Provided::Nothing);

        assert!(matches!(
            events.as_slice(),
            [AgentEvent::MessageStarted { .. }]
        ));
    }
}
