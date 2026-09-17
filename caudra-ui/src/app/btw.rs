use std::borrow::Cow;
use std::sync::Arc;

use caudra_agent::CancelToken;
use caudra_providers::{
    AgentError, CacheKey, Message, ProviderEvent, StopReason, project_messages,
};
use caudra_storage::id::SessionRef;
use flume::Sender;
use futures_lite::future;

use crate::agent::BtwPrompt;
use crate::components::stream_modal::{StreamEvent, StreamUsage};
use caudra_storage::usage_ledger::LedgerPurpose;

use super::App;

const TITLE: &str = " /btw ";

const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly in a \
single response.\n- Do NOT call any tool. No tool result will come back, so a tool call wastes \
the turn.\n- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing \
conversation context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If \
you don't know, say so; do not offer to look it up.\n</system-reminder>";

const TOOL_CALL_STOPPED: &str = "\n\n_Stopped: the model tried to call a tool. `/btw` cannot run \
tools, so ask in the main session instead._";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
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
        let mut messages = match caudra_agent::project_for_target(
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
        caudra_agent::close_dangling_tool_calls(&mut messages, caudra_agent::UNAVAILABLE_RESULT);
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal
            .open(TITLE, format!("Q: {question}"), false, rx, trigger);

        // A btw forks the main conversation, so it shares its cache key on purpose.
        let cache_key = CacheKey::session(&SessionRef::from(self.state.session.id));
        smol::spawn(run_btw(prompt, messages, tx, Some(cache_key), cancel)).detach();
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
            let _ = btw_tx.send(StreamEvent::Done(StreamUsage {
                cost: model.billed_cost(&response.usage, opts.fast),
                billing: model.billing,
                usage: response.usage,
                model: model.id.clone(),
                provider: model.provider.to_string(),
                purpose: LedgerPurpose::Btw,
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

    use super::*;

    const Q: &str = "why sqlite?";
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
                    message: Message::default(),
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
    fn a_captured_prompt_keeps_its_model_provider_pair_across_a_switch() {
        smol::block_on(async {
            let (first_tx, first_rx) = flume::unbounded();
            let (second_tx, second_rx) = flume::unbounded();
            let shared = ArcSwap::from_pointee(prompt(FIRST_MODEL, first_tx));
            let captured = shared.load_full();
            shared.store(Arc::new(prompt(SECOND_MODEL, second_tx)));
            let (event_tx, event_rx) = flume::unbounded();

            run_btw(
                captured,
                vec![Message::user(Q.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            assert_eq!(first_rx.try_recv().unwrap(), FIRST_MODEL);
            assert!(second_rx.try_recv().is_err());
            assert!(matches!(event_rx.try_recv(), Ok(StreamEvent::Done(_))));
        });
    }
}
