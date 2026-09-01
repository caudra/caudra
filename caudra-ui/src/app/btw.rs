use std::borrow::Cow;
use std::sync::Arc;

use caudra_agent::CancelToken;
use caudra_providers::provider::Provider;
use caudra_providers::{AgentError, Message, Model, ProviderEvent, StopReason, project_messages};
use caudra_storage::id::SessionRef;
use flume::Sender;
use futures_lite::future;

use crate::agent::BtwPrompt;
use crate::components::btw_modal::{BtwEvent, BtwUsage};

use super::App;

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
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
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
        let mut messages = match caudra_agent::project_for_provider(&messages, &prompt.tools) {
            Cow::Borrowed(_) => messages,
            Cow::Owned(projected) => projected,
        };
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        caudra_agent::close_dangling_tool_calls(&mut messages, caudra_agent::UNAVAILABLE_RESULT);
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.btw_modal.open(&question, rx, trigger);

        let session_id = SessionRef::from(self.state.session.id);
        smol::spawn(run_btw(
            provider,
            model,
            prompt,
            messages,
            tx,
            Some(session_id),
            cancel,
        ))
        .detach();
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    prompt: Arc<BtwPrompt>,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    session_id: Option<SessionRef>,
    cancel: CancelToken,
) {
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
                if btw_tx.send(BtwEvent::TextDelta(delta)).is_err() {
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
            session_id.as_ref(),
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
                let _ = btw_tx.send(BtwEvent::TextDelta(TOOL_CALL_STOPPED.into()));
            }
            let _ = btw_tx.send(BtwEvent::Done(BtwUsage {
                cost: model.billed_cost(&response.usage, opts.fast),
                usage: response.usage,
                model: model.id.clone(),
            }));
        }
        // The receiver is already gone, which is what cancelled the stream.
        Err(AgentError::Cancelled) => {}
        Err(error) => {
            let _ = btw_tx.send(BtwEvent::Error(error.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "why sqlite?";

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
}
