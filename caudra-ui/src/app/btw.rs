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
use crate::components::prompt_progress::PromptProgress;
use crate::components::stream_modal::{StreamDone, StreamEvent, StreamFooter, StreamUsage};
use crate::components::{DisplayMessage, DisplayRole};

use super::{App, HISTORY_UNREADABLE};

const TITLE: &str = " /btw ";
pub(super) const CLARIFICATION_MAIN_ONLY: &str = "Ask /btw is available for main-session questions only; task question context is not supported yet";
const PROMPT_INITIALIZING: &str = "System prompt is still initializing";
const CLARIFICATION_REMINDER: &str = "The user is clarifying a pending question form, not answering it. \
The asking agent is still waiting. Explain the question or choices; do not select, submit, or change \
answers. The following is the question currently being viewed, not a user decision:";
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
    clarification_context: Option<String>,
}

impl BtwThread {
    /// Opens over exactly what the live request sends for `history`, so the
    /// provider can reuse the cached prefix instead of re-reading the whole
    /// history as fresh input tokens. That includes answering a call the
    /// mirror caught mid-turn, which a provider would otherwise reject.
    fn new(prompt: Arc<BtwPrompt>, history: Vec<Message>) -> Self {
        let transport = prompt.provider.reasoning_transport(&prompt.model);
        let base = match caudra_agent::project_request(
            &history,
            &prompt.tools,
            &prompt.model,
            transport,
        ) {
            Cow::Borrowed(_) => history,
            Cow::Owned(projected) => projected,
        };
        Self {
            prompt,
            base,
            exchanges: Vec::new(),
            pending: None,
            clarification_context: None,
        }
    }

    /// A question that never settled is replaced rather than kept: the request
    /// that carried it failed, so the model never answered it.
    fn ask(&mut self, question: String) {
        self.pending = Some(question);
    }

    fn ask_clarification(&mut self, question: String, context: String) {
        let changed = self.clarification_context.as_ref() != Some(&context)
            || self.pending.is_some()
            || self.exchanges.is_empty();
        let question = if changed {
            format!("{CLARIFICATION_REMINDER}\n\n{context}\n\n{question}")
        } else {
            question
        };
        self.clarification_context = Some(context);
        self.ask(question);
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
        } else {
            self.clarification_context = None;
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
    pub(super) fn open_question_btw(&mut self) {
        if !self.question_form.is_open() {
            return;
        }
        if self.question_subagent.is_some() || self.task_interactions.question.is_some() {
            self.flash(CLARIFICATION_MAIN_ONLY.into());
            return;
        }
        if !self
            .btw_prompt
            .as_ref()
            .is_some_and(|prompt| !prompt.load().system.is_empty())
        {
            self.flash(PROMPT_INITIALIZING.into());
            return;
        }
        if !self.stream_modal.is_clarification() {
            self.close_stream_modal();
        } else {
            self.settle_stream_modal();
        }
        self.autoscroll = None;
        self.selection_state = None;
        self.stream_modal
            .open_clarification(self.question_form.clarification_context());
    }

    pub(super) fn discard_question_btw(&mut self) {
        if self.stream_modal.is_clarification() {
            self.close_stream_modal();
        }
    }

    pub(super) fn close_stream_modal(&mut self) {
        let _ = self.stream_modal.poll();
        self.settle_stream_modal();
        self.stream_modal.close();
        self.end_btw_thread();
    }

    fn capture_btw_thread(&mut self) -> Option<BtwThread> {
        let items = self
            .shared_history
            .as_ref()
            .map(|h| Vec::clone(&h.load().messages))
            .unwrap_or_default();
        let messages = match project_messages(&items) {
            Ok(messages) => messages,
            Err(error) => {
                self.status_bar
                    .flash(format!("{HISTORY_UNREADABLE}{error}"));
                return None;
            }
        };
        let Some(prompt) = self
            .btw_prompt
            .as_ref()
            .map(|p| p.load_full())
            .filter(|p| !p.system.is_empty())
        else {
            self.status_bar.flash(PROMPT_INITIALIZING.into());
            return None;
        };
        Some(BtwThread::new(prompt, messages))
    }

    fn install_btw_thread(&mut self, thread: BtwThread) {
        self.btw_thread = Some(thread);
        // Streaming text is not in history yet and draws below every message,
        // so a marker appended now lands exactly where the snapshot cuts.
        self.main_chat().push(DisplayMessage::new(
            DisplayRole::Notice,
            BTW_CUTOFF_MARKER.into(),
        ));
    }

    pub(crate) fn start_btw(&mut self, question: String) {
        let Some(mut thread) = self.capture_btw_thread() else {
            return;
        };
        self.renew_question_wait();
        self.close_stream_modal();
        thread.ask(question.clone());
        self.install_btw_thread(thread);

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
        self.settle_stream_modal();
        let context = self
            .stream_modal
            .is_clarification()
            .then(|| self.question_form.clarification_context());
        if context.is_some() && self.btw_thread.is_none() {
            let Some(thread) = self.capture_btw_thread() else {
                self.stream_modal.handle_paste(&question);
                return;
            };
            self.install_btw_thread(thread);
        }
        let Some(thread) = self.btw_thread.as_mut() else {
            return;
        };
        if let Some(context) = context {
            thread.ask_clarification(question.clone(), context);
        } else {
            thread.ask(question.clone());
        }
        self.renew_question_wait();
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
    let (event_tx, event_rx) = flume::unbounded();

    let forwarder = smol::spawn({
        let btw_tx = btw_tx.clone();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let forwarded = match event {
                    ProviderEvent::TextDelta { text } => StreamEvent::TextDelta(text),
                    ProviderEvent::ThinkingDelta { text } => StreamEvent::ThinkingDelta(text),
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
                if btw_tx.send(forwarded).is_err() {
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
    use caudra_agent::UNAVAILABLE_RESULT;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{Model, ModelInfo, RequestOptions, StreamResponse};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    const Q: &str = "why sqlite?";
    const FOLLOW_UP: &str = "and why not postgres?";
    const ANSWER: &str = "it ships in the binary";
    const BASE: &str = "let's pick a database";
    const FIRST_MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const SECOND_MODEL: &str = "openai/gpt-5.4";
    const SYSTEM: &str = "system";
    const PREFILL_PROCESSED: u32 = 1_200;
    const PREFILL_TOTAL: u32 = 4_000;
    const PREFILL_CACHE: u32 = 900;
    const TOOL: &str = "file_read";
    const ORPHANED_CALL: &str = "orphaned";
    const UNANSWERED_CALL: &str = "unanswered";
    const OPEN_CALLS: [&str; 2] = ["open-first", "open-second"];
    const THINKING_DELTAS: [&str; 2] = ["considering ", "deployment"];
    const TEXT_DELTAS: [&str; 2] = ["it ships ", "in the binary"];
    const EXPECTED_DONE: &str = "expected stream completion after all deltas";
    const CLARIFY_STORAGE: &str = "About: Q1/2 · Storage\nWhich database?\nSQLite\nPostgreSQL";
    const CLARIFY_DEPLOYMENT: &str = "About: Q2/2 · Deployment\nWhere should it run?\nLocal\nCloud";

    #[test_case(false ; "same_question")]
    #[test_case(true ; "different_question")]
    fn clarification_context_follows_focus_without_repeating_unchanged_questions(changed: bool) {
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, called)),
            vec![Message::user(BASE.into())],
        );
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        let initial = thread.request_messages();
        let initial_question = user_text(initial.last().unwrap());
        assert!(initial_question.contains(CLARIFICATION_REMINDER));
        assert!(initial_question.contains(CLARIFY_STORAGE));
        assert!(initial_question.ends_with(Q));
        thread.settle(Some(ANSWER.into()));

        let context = if changed {
            CLARIFY_DEPLOYMENT
        } else {
            CLARIFY_STORAGE
        };
        thread.ask_clarification(FOLLOW_UP.into(), context.into());
        let next = thread.request_messages();
        assert_eq!(user_text(&next[0]), BASE);
        assert_eq!(user_text(&next[1]), initial_question);
        let follow_up = user_text(next.last().unwrap());
        if changed {
            assert!(follow_up.contains(CLARIFY_DEPLOYMENT));
        } else {
            assert_eq!(follow_up, FOLLOW_UP);
        }
    }

    #[test_case(false ; "interrupted")]
    #[test_case(true ; "failed")]
    fn clarification_retry_keeps_context_that_never_settled(failed: bool) {
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(Arc::new(prompt(FIRST_MODEL, called)), Vec::new());
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        thread.settle(Some(ANSWER.into()));
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        if failed {
            thread.settle(None);
        }
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        let messages = thread.request_messages();
        assert!(user_text(messages.last().unwrap()).contains(CLARIFY_DEPLOYMENT));
        assert_eq!(thread.exchange_count(), 1);
    }

    /// Hands over the route and messages of every request it answers.
    struct RecordingProvider(flume::Sender<(String, Vec<Message>)>);

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.0.send((model.spec(), messages.to_vec())).unwrap();
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

    /// Reports prefill before it answers, as OpenAI's Responses provider does.
    struct PrefillingProvider;

    impl Provider for PrefillingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                event_tx.send(ProviderEvent::PromptProgress {
                    processed: PREFILL_PROCESSED,
                    total: PREFILL_TOTAL,
                    cache: PREFILL_CACHE,
                })?;
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

    struct ReasoningProvider {
        interleaved: bool,
    }

    impl Provider for ReasoningProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let mut events = [
                    ProviderEvent::ThinkingDelta {
                        text: THINKING_DELTAS[0].into(),
                    },
                    ProviderEvent::ThinkingDelta {
                        text: THINKING_DELTAS[1].into(),
                    },
                    ProviderEvent::TextDelta {
                        text: TEXT_DELTAS[0].into(),
                    },
                    ProviderEvent::TextDelta {
                        text: TEXT_DELTAS[1].into(),
                    },
                ];
                if self.interleaved {
                    events.swap(1, 2);
                }
                for event in events {
                    event_tx.send(event)?;
                }
                let mut message = assistant_text(ANSWER);
                message.content.insert(
                    0,
                    ContentBlock::Thinking {
                        thinking: THINKING_DELTAS.concat(),
                        signature: None,
                        duration_ms: None,
                        interrupted: false,
                        responses: None,
                    },
                );
                Ok(StreamResponse {
                    message,
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn prompt_with(provider: Arc<dyn Provider>, spec: &str) -> BtwPrompt {
        BtwPrompt {
            provider,
            model: Model::from_spec(spec).unwrap(),
            system: SYSTEM.into(),
            tools: json!([]),
            opts: RequestOptions::default(),
        }
    }

    fn prompt(spec: &str, called: flume::Sender<(String, Vec<Message>)>) -> BtwPrompt {
        prompt_with(Arc::new(RecordingProvider(called)), spec)
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

    /// The forwarder used to keep text alone, which left the modal with
    /// nothing to draw for the whole of a long prefill.
    #[test]
    fn prefill_progress_reaches_the_modal() {
        smol::block_on(async {
            let (event_tx, event_rx) = flume::unbounded();
            run_btw(
                Arc::new(prompt_with(Arc::new(PrefillingProvider), FIRST_MODEL)),
                vec![Message::user(Q.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            assert!(matches!(
                event_rx.try_recv(),
                Ok(StreamEvent::Progress(progress))
                    if progress.processed == PREFILL_PROCESSED
                        && progress.total == PREFILL_TOTAL
                        && progress.cache == PREFILL_CACHE
            ));
        });
    }

    #[test_case(false; "split_reasoning_before_text")]
    #[test_case(true; "interleaved_reasoning_and_text")]
    fn reasoning_streams_separately_and_stays_out_of_follow_up_history(interleaved: bool) {
        smol::block_on(async {
            let prompt = Arc::new(prompt_with(
                Arc::new(ReasoningProvider { interleaved }),
                FIRST_MODEL,
            ));
            let mut thread = BtwThread::new(prompt, vec![Message::user(BASE.into())]);
            thread.ask(Q.into());
            let (event_tx, event_rx) = flume::unbounded();
            run_btw(
                Arc::clone(&thread.prompt),
                thread.request_messages(),
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            let expected = if interleaved {
                [
                    (true, THINKING_DELTAS[0]),
                    (false, TEXT_DELTAS[0]),
                    (true, THINKING_DELTAS[1]),
                    (false, TEXT_DELTAS[1]),
                ]
            } else {
                [
                    (true, THINKING_DELTAS[0]),
                    (true, THINKING_DELTAS[1]),
                    (false, TEXT_DELTAS[0]),
                    (false, TEXT_DELTAS[1]),
                ]
            };
            for (thinking, text) in expected {
                assert!(matches!(
                    (event_rx.try_recv().unwrap(), thinking),
                    (StreamEvent::ThinkingDelta(delta), true)
                        | (StreamEvent::TextDelta(delta), false) if delta == text
                ));
            }
            let StreamEvent::Done(done) = event_rx.try_recv().unwrap() else {
                panic!("{EXPECTED_DONE}");
            };
            assert_eq!(done.answer.as_deref(), Some(ANSWER));
            assert!(event_rx.try_recv().is_err());

            thread.settle(done.answer);
            assert!(thread.pending().is_none());
            assert_eq!(thread.exchanges, vec![(Q.to_owned(), ANSWER.to_owned())]);
            thread.ask(FOLLOW_UP.into());
            let expected = vec![
                Message::user(BASE.into()),
                btw_question(Q),
                assistant_text(ANSWER),
                Message::user(FOLLOW_UP.into()),
            ];
            assert_eq!(
                serde_json::to_value(thread.request_messages()).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        });
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
            assert_eq!(first_rx.try_recv().unwrap().0, FIRST_MODEL);
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
                first_rx.try_recv().unwrap().0,
                FIRST_MODEL,
                "a follow-up rides the same captured route"
            );
            assert!(second_rx.try_recv().is_err());
        });
    }

    /// A thread goes out exactly as the live request would, repairs included,
    /// rather than through a hand-rolled copy of the projection that drifts.
    #[test]
    fn a_thread_sends_the_request_projection_and_the_question() {
        smol::block_on(async {
            let history = vec![
                Message::user(BASE.into()),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: ORPHANED_CALL.into(),
                        content: ANSWER.into(),
                        is_error: false,
                        output_ref: None,
                    }],
                    ..Default::default()
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(UNANSWERED_CALL, TOOL, json!({}))],
                    ..Default::default()
                },
                Message::user(FOLLOW_UP.into()),
                assistant_text(ANSWER),
            ];
            let (called, requests) = flume::unbounded();
            let mut thread = BtwThread::new(Arc::new(prompt(FIRST_MODEL, called)), history.clone());
            thread.ask(Q.into());
            let (event_tx, _event_rx) = flume::unbounded();

            run_btw(
                Arc::clone(&thread.prompt),
                thread.request_messages(),
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            let pinned = &thread.prompt;
            let mut expected = caudra_agent::project_request(
                &history,
                &pinned.tools,
                &pinned.model,
                pinned.provider.reasoning_transport(&pinned.model),
            )
            .into_owned();
            expected.push(btw_question(Q));
            let (_, sent) = requests.try_recv().unwrap();
            assert_eq!(
                serde_json::to_value(sent).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        });
    }

    /// Reading the history rejects every broken tool pairing but one: a turn
    /// caught between its calls and their results. A provider rejects a call
    /// left unanswered, so the thread closes each one, in the order the turn
    /// made them.
    #[test_case(1 ; "one_open_call")]
    #[test_case(2 ; "two_open_calls")]
    fn a_thread_closes_the_calls_a_turn_left_open(open: usize) {
        let calls = &OPEN_CALLS[..open];
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, called)),
            vec![
                Message::user(BASE.into()),
                Message {
                    role: Role::Assistant,
                    content: calls
                        .iter()
                        .map(|id| ContentBlock::tool_use(*id, TOOL, json!({})))
                        .collect(),
                    ..Default::default()
                },
            ],
        );
        thread.ask(Q.into());

        let messages = thread.request_messages();
        let (_question, base) = messages.split_last().unwrap();
        let closing = base.last().unwrap();
        assert!(matches!(closing.role, Role::User));
        assert_eq!(closing.display_text.as_deref(), Some(""));
        let results: Vec<(&str, &str, bool)> = closing
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            calls
                .iter()
                .map(|id| (*id, UNAVAILABLE_RESULT, true))
                .collect::<Vec<_>>()
        );
    }
}
