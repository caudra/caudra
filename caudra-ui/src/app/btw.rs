use std::borrow::Cow;
use std::sync::Arc;

use caudra_agent::CancelToken;
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, Message, ProviderEvent, Role, StopReason, project_messages,
};
use caudra_storage::id::SessionRef;
use caudra_storage::usage_ledger::LedgerPurpose;
use flume::Sender;
use futures_lite::future;

use crate::agent::BtwPrompt;
use crate::components::stream_modal::{StreamDone, StreamEvent, StreamFooter, StreamUsage};
use crate::components::{DisplayMessage, DisplayRole};

use super::App;

const TITLE: &str = " /btw ";
/// Drawn where the thread's snapshot of the conversation ends, so the reader
/// can tell what the side question could and could not see.
pub(crate) const BTW_CUTOFF_MARKER: &str = "/btw reads the conversation up to here";

const BTW_REMINDER: &str = "<system-reminder>\n# Side question\n\nThe user is asking beside \
the conversation, not continuing it. Answer from the conversation so far, directly and in one \
response.\n- Do NOT call any tool. No result will ever come back, so a call wastes the \
answer.\n- Nothing in this thread enters the main conversation, and the work it discusses is \
not yours to start.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If \
you don't know, say so; do not offer to look it up.\n- Follow-up questions may arrive in this \
thread. Answer each the same way.\n</system-reminder>";

const TOOL_CALL_STOPPED: &str = "\n\n_Stopped: the model tried to call a tool. `/btw` cannot run \
tools, so ask in the main session instead._";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        ..Default::default()
    }
}

/// The answer as text alone. Reasoning is the model's own and a tool call
/// never ran, so neither belongs in the thread the next question extends.
fn answer_text(message: &Message) -> Option<String> {
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// A side conversation forked from the main one: the prefix and history it
/// opened over, pinned, and the text-only exchanges asked on top of them.
/// Nothing here is persisted; it lives exactly as long as the modal.
pub(crate) struct BtwThread {
    prompt: Arc<BtwPrompt>,
    base: Vec<Message>,
    exchanges: Vec<(String, String)>,
    pending: Option<String>,
}

impl BtwThread {
    fn new(prompt: Arc<BtwPrompt>, base: Vec<Message>) -> Self {
        Self {
            prompt,
            base,
            exchanges: Vec::new(),
            pending: None,
        }
    }

    /// A question that never settled is replaced rather than kept: the request
    /// that carried it failed, so the model never answered it.
    fn ask(&mut self, question: String) {
        self.pending = Some(question);
    }

    #[cfg(test)]
    pub(super) fn exchange_count(&self) -> usize {
        self.exchanges.len()
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// Files the pending question with its answer. No answer, which is what a
    /// refused or tool-calling reply leaves, drops the question instead, so the
    /// thread stays a strict alternation the provider accepts.
    fn settle(&mut self, answer: Option<String>) {
        let Some(question) = self.pending.take() else {
            return;
        };
        if let Some(answer) = answer {
            self.exchanges.push((question, answer));
        }
    }

    /// The reminder rides the first question only: a later block would say
    /// nothing new and would move the cache boundary for every follow-up.
    fn request_messages(&self) -> Vec<Message> {
        let mut messages = self.base.clone();
        let questions = self
            .exchanges
            .iter()
            .map(|(question, answer)| (question.as_str(), Some(answer.as_str())))
            .chain(self.pending.as_deref().map(|question| (question, None)));
        for (index, (question, answer)) in questions.enumerate() {
            messages.push(if index == 0 {
                btw_question(question)
            } else {
                Message::user(question.to_owned())
            });
            if let Some(answer) = answer {
                messages.push(assistant_text(answer));
            }
        }
        messages
    }
}

impl App {
    pub(crate) fn start_btw(&mut self, question: String) {
        let items = self
            .shared_history
            .as_ref()
            .map(|h| Vec::clone(&h.load().messages))
            .unwrap_or_default();
        let messages = match project_messages(&items) {
            Ok(messages) => messages,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to read session history: {error}"));
                return;
            }
        };
        let Some(prompt) = self
            .btw_prompt
            .as_ref()
            .map(|p| p.load_full())
            .filter(|p| !p.system.is_empty())
        else {
            self.status_bar
                .flash("System prompt is still initializing".into());
            return;
        };

        // Mirrors what the live request puts on the wire, so the provider can reuse the cached
        // prefix instead of re-reading the whole history as fresh input tokens.
        let transport = prompt.provider.reasoning_transport(&prompt.model);
        let mut base = match caudra_agent::project_for_target(
            &messages,
            &prompt.tools,
            &prompt.model,
            transport,
        ) {
            Cow::Borrowed(_) => messages,
            Cow::Owned(projected) => projected,
        };
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        caudra_agent::close_dangling_tool_calls(&mut base, caudra_agent::UNAVAILABLE_RESULT);

        self.end_btw_thread();
        let mut thread = BtwThread::new(prompt, base);
        thread.ask(question.clone());
        self.btw_thread = Some(thread);
        // Streaming text is not in history yet and draws below every message,
        // so a marker appended now lands exactly where the snapshot cuts.
        self.main_chat().push(DisplayMessage::new(
            DisplayRole::Notice,
            BTW_CUTOFF_MARKER.into(),
        ));

        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal.open(
            TITLE,
            format!("Q: {question}"),
            StreamFooter::FollowUp,
            rx,
            trigger,
        );
        self.spawn_btw(tx, cancel);
    }

    /// Asks the next question over the same snapshot and prefix, so the whole
    /// thread stays one cached lineage.
    pub(crate) fn continue_btw(&mut self, question: String) {
        let Some(thread) = self.btw_thread.as_mut() else {
            return;
        };
        thread.ask(question.clone());
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal
            .begin_exchange(format!("Q: {question}"), rx, trigger);
        self.spawn_btw(tx, cancel);
    }

    fn spawn_btw(&self, tx: Sender<StreamEvent>, cancel: CancelToken) {
        let Some(thread) = &self.btw_thread else {
            return;
        };
        // A btw forks the main conversation, so it shares its cache key on purpose.
        let cache_key = CacheKey::session(&SessionRef::from(self.state.session.id));
        smol::spawn(run_btw(
            Arc::clone(&thread.prompt),
            thread.request_messages(),
            tx,
            Some(cache_key),
            cancel,
        ))
        .detach();
    }

    pub(crate) fn settle_btw(&mut self, answer: Option<String>) {
        if let Some(thread) = self.btw_thread.as_mut() {
            thread.settle(answer);
        }
    }

    /// Forgets the thread and takes its marker out of the transcript. Every
    /// way the modal closes reaches here through the tick, so no close path
    /// has to know about the marker.
    pub(crate) fn end_btw_thread(&mut self) {
        if self.btw_thread.take().is_some() {
            self.main_chat().remove_notice(BTW_CUTOFF_MARKER);
        }
    }
}

async fn run_btw(
    prompt: Arc<BtwPrompt>,
    messages: Vec<Message>,
    btw_tx: Sender<StreamEvent>,
    cache_key: Option<CacheKey>,
    cancel: CancelToken,
) {
    let provider = Arc::clone(&prompt.provider);
    let model = prompt.model.clone();
    // btw bypasses `stream_with_retry`, the only other place options meet a model, so it has to
    // clamp for itself or it sends options the live request would have gated away.
    let opts = prompt.opts.clamped(&model);
    let messages = caudra_providers::adapt_images_for_model(&model, &messages);
    let (event_tx, event_rx) = flume::unbounded();

    let forwarder = smol::spawn({
        let btw_tx = btw_tx.clone();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let delta = match event {
                    ProviderEvent::TextDelta { text } | ProviderEvent::ThinkingDelta { text } => {
                        text
                    }
                    _ => continue,
                };
                if btw_tx.send(StreamEvent::TextDelta(delta)).is_err() {
                    return;
                }
            }
        }
    });

    let result = future::race(
        provider.stream_message(
            &model,
            &messages,
            &prompt.system,
            &prompt.tools,
            &event_tx,
            opts.clone(),
            cache_key.as_ref(),
        ),
        async {
            cancel.cancelled().await;
            Err(AgentError::Cancelled)
        },
    )
    .await;
    drop(event_tx);
    forwarder.await;

    match result {
        Ok(response) => {
            if response.stop_reason == Some(StopReason::ToolUse) {
                let _ = btw_tx.send(StreamEvent::TextDelta(TOOL_CALL_STOPPED.into()));
            }
            let _ = btw_tx.send(StreamEvent::Done(StreamDone {
                usage: StreamUsage {
                    cost: model.billed_cost(&response.usage, opts.fast),
                    billing: model.billing,
                    usage: response.usage,
                    model: model.id.clone(),
                    provider: model.provider.to_string(),
                    purpose: LedgerPurpose::Btw,
                },
                answer: answer_text(&response.message),
            }));
        }
        // The receiver is already gone, which is what cancelled the stream.
        Err(AgentError::Cancelled) => {}
        Err(error) => {
            let _ = btw_tx.send(StreamEvent::Error(error.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{Model, RequestOptions, StreamResponse};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const Q: &str = "why sqlite?";
    const FOLLOW_UP: &str = "and why not postgres?";
    const ANSWER: &str = "it ships in the binary";
    const BASE: &str = "let's pick a database";
    const FIRST_MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const SECOND_MODEL: &str = "openai/gpt-5.4";
    const SYSTEM: &str = "system";

    struct RecordingProvider(flume::Sender<String>);

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.0.send(model.spec()).unwrap();
                Ok(StreamResponse {
                    message: assistant_text(ANSWER),
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn prompt(spec: &str, called: flume::Sender<String>) -> BtwPrompt {
        BtwPrompt {
            provider: Arc::new(RecordingProvider(called)),
            model: Model::from_spec(spec).unwrap(),
            system: SYSTEM.into(),
            tools: json!([]),
            opts: RequestOptions::default(),
        }
    }

    fn thread() -> BtwThread {
        let (tx, _rx) = flume::unbounded();
        BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, tx)),
            vec![Message::user(BASE.into())],
        )
    }

    fn user_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                caudra_providers::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn injects_reminder_before_question() {
        let text = user_text(&btw_question(Q));
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }

    #[test]
    fn reminder_forbids_tools_without_claiming_they_are_absent() {
        assert!(
            BTW_REMINDER.contains("Do NOT call any tool"),
            "tool definitions are now sent, so the reminder must forbid rather than deny them"
        );
        assert!(
            !BTW_REMINDER.contains("NO tools"),
            "claiming there are no tools contradicts the definitions on the wire"
        );
    }

    #[test]
    fn a_thread_extends_its_base_with_exchanges_and_the_pending_question() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.settle(Some(ANSWER.into()));
        thread.ask(FOLLOW_UP.into());

        let messages = thread.request_messages();
        let texts: Vec<(bool, String)> = messages
            .iter()
            .map(|m| (matches!(m.role, Role::Assistant), user_text(m)))
            .collect();
        assert_eq!(texts.len(), 4);
        assert_eq!(texts[0], (false, BASE.to_owned()));
        assert!(
            !texts[1].0 && texts[1].1.starts_with(BTW_REMINDER) && texts[1].1.ends_with(Q),
            "the reminder rides the first question"
        );
        assert_eq!(texts[2], (true, ANSWER.to_owned()));
        assert_eq!(
            texts[3],
            (false, FOLLOW_UP.to_owned()),
            "a follow-up carries no second reminder"
        );
    }

    #[test_case(None ; "no_answer")]
    #[test_case(Some("  \n") ; "blank_answer_is_no_answer")]
    fn an_unanswered_question_leaves_the_thread(answer: Option<&str>) {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.settle(
            answer
                .map(str::to_owned)
                .and_then(|a| answer_text(&assistant_text(&a))),
        );
        assert!(thread.exchanges.is_empty());
        assert!(thread.pending.is_none());
        assert_eq!(thread.request_messages().len(), 1, "only the base remains");
    }

    #[test]
    fn a_question_asked_over_a_failed_one_replaces_it() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.ask(FOLLOW_UP.into());
        let messages = thread.request_messages();
        assert_eq!(messages.len(), 2);
        assert!(user_text(&messages[1]).ends_with(FOLLOW_UP));
    }

    #[test]
    fn answer_text_keeps_text_blocks_only() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                    duration_ms: None,
                    interrupted: false,
                    responses: None,
                },
                ContentBlock::Text {
                    text: ANSWER.into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "file_read".into(),
                    input: json!({}),
                    thought_signature: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(answer_text(&message).as_deref(), Some(ANSWER));
        assert_eq!(answer_text(&Message::default()), None);
    }

    #[test]
    fn a_captured_prompt_keeps_its_model_provider_pair_across_a_switch() {
        smol::block_on(async {
            let (first_tx, first_rx) = flume::unbounded();
            let (second_tx, second_rx) = flume::unbounded();
            let shared = ArcSwap::from_pointee(prompt(FIRST_MODEL, first_tx));
            let captured = shared.load_full();
            shared.store(Arc::new(prompt(SECOND_MODEL, second_tx)));
            let (event_tx, event_rx) = flume::unbounded();

            run_btw(
                Arc::clone(&captured),
                vec![Message::user(Q.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;
            assert_eq!(first_rx.try_recv().unwrap(), FIRST_MODEL);
            assert!(matches!(
                event_rx.try_recv(),
                Ok(StreamEvent::Done(StreamDone { answer: Some(answer), .. })) if answer == ANSWER
            ));

            let (event_tx, _event_rx) = flume::unbounded();
            run_btw(
                captured,
                vec![Message::user(FOLLOW_UP.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;
            assert_eq!(
                first_rx.try_recv().unwrap(),
                FIRST_MODEL,
                "a follow-up rides the same captured route"
            );
            assert!(second_rx.try_recv().is_err());
        });
    }
}
