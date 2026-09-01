use std::time::{Duration, Instant};

use caudra_providers::provider::Provider;
use caudra_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use caudra_providers::{
    ContentBlock, Message, Model, ProviderEvent, RequestOptions, StreamResponse,
};
use caudra_storage::id::SessionRef;
use serde_json::Value;
use tracing::warn;

use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

const FUNCTIONS_PREFIX: &str = "functions.";

/// GPT models sometimes emit `functions.<name>`, a Codex training habit.
/// Stripped here at the provider boundary so no raw name enters the agent;
/// the batch plugin mirrors the rule in Lua.
pub(crate) fn canonical_tool_name(name: &str) -> &str {
    name.strip_prefix(FUNCTIONS_PREFIX).unwrap_or(name)
}

fn canonicalize_tool_names(message: &mut Message) {
    for block in &mut message.content {
        if let ContentBlock::ToolUse { name, .. } = block {
            *name = canonical_tool_name(name).to_owned();
        }
    }
}

/// Streamed assistant text plus one duration per contiguous run of thinking
/// deltas, in emission order.
struct ForwardedStream {
    streamed: String,
    reasoning: Vec<Duration>,
}

/// Only Anthropic's SSE parser exposes reasoning block boundaries, so the
/// durations are timed here instead, where every provider looks the same: a
/// run starts on the first thinking delta and ends when anything else
/// arrives or the stream closes.
async fn forward_provider_events(
    prx: flume::Receiver<ProviderEvent>,
    event_tx: Option<&EventSender>,
) -> ForwardedStream {
    let mut streamed = String::new();
    let mut reasoning = Vec::new();
    let mut run_started: Option<Instant> = None;
    while let Ok(pe) = prx.recv_async().await {
        if matches!(pe, ProviderEvent::ThinkingDelta { .. }) {
            run_started.get_or_insert_with(Instant::now);
        } else if let Some(started) = run_started.take() {
            reasoning.push(started.elapsed());
        }
        let ae = match pe {
            ProviderEvent::TextDelta { text } => {
                streamed.push_str(&text);
                AgentEvent::TextDelta { text }
            }
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ToolUseStart { id, name } => AgentEvent::ToolPending {
                id,
                name: canonical_tool_name(&name).to_owned(),
            },
            ProviderEvent::PromptProgress {
                processed,
                total,
                cache,
            } => AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            },
        };
        if event_tx.is_some_and(|event_tx| event_tx.send(ae).is_err()) {
            break;
        }
    }
    if let Some(started) = run_started {
        reasoning.push(started.elapsed());
    }
    ForwardedStream {
        streamed,
        reasoning,
    }
}

/// Providers append thinking blocks in the order their deltas arrive, so the
/// n-th timed run belongs to the n-th thinking block. A mismatch leaves the
/// extra blocks untimed rather than mispairing them.
fn attach_reasoning_durations(message: &mut Message, durations: &[Duration]) {
    let blocks = message
        .content
        .iter_mut()
        .filter_map(|block| match block {
            ContentBlock::Thinking { duration_ms, .. } => Some(duration_ms),
            _ => None,
        })
        .take(durations.len());
    for (slot, duration) in blocks.zip(durations) {
        *slot = Some(duration.as_millis().min(u128::from(u64::MAX)) as u64);
    }
}

/// Cancelling mid-stream carries the text the user still sees on screen,
/// so the caller can keep it in history. A cancel during the retry backoff
/// carries nothing: the `Retry` event already made the view drop the failed
/// attempt's text (`stream_reset`), and history must agree with the view.
#[derive(Debug)]
pub(crate) enum StreamError {
    Cancelled { streamed: String },
    Other(AgentError),
}

impl From<AgentError> for StreamError {
    fn from(e: AgentError) -> Self {
        Self::Other(e)
    }
}

impl From<StreamError> for AgentError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Cancelled { .. } => Self::Cancelled,
            StreamError::Other(e) => e,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &EventSender,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, StreamError> {
    stream_with_retry_inner(
        provider,
        messages,
        model,
        system,
        tools,
        Some(event_tx),
        cancel,
        opts,
        session_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_silent_with_retry(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, StreamError> {
    stream_with_retry_inner(
        provider, messages, model, system, tools, None, cancel, opts, session_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_with_retry_inner(
    provider: &dyn Provider,
    messages: &[Message],
    model: &Model,
    system: &str,
    tools: &Value,
    event_tx: Option<&EventSender>,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, StreamError> {
    let opts = opts.clamped(model);
    let messages = caudra_providers::adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut retry = RetryState::new();
    loop {
        let started = Instant::now();
        let (ptx, prx) = flume::unbounded();
        let forwarder = smol::spawn({
            let event_tx = event_tx.cloned();
            async move { forward_provider_events(prx, event_tx.as_ref()).await }
        });
        let result = futures_lite::future::race(
            provider.stream_message(
                model,
                messages,
                system,
                tools,
                &ptx,
                opts.clone(),
                session_id,
            ),
            async {
                cancel.cancelled().await;
                Err(AgentError::Cancelled)
            },
        )
        .await;
        drop(ptx);
        let ForwardedStream {
            streamed,
            reasoning,
        } = forwarder.await;
        match result {
            Ok(mut r) => {
                canonicalize_tool_names(&mut r.message);
                attach_reasoning_durations(&mut r.message, &reasoning);
                emit_api_request(model, &r, opts, started.elapsed());
                return Ok(r);
            }
            Err(AgentError::Cancelled) => return Err(StreamError::Cancelled { streamed }),
            Err(e) if e.is_retryable() => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                if e.should_rotate_key()
                    && let Ok(true) = provider.rotate_key().await
                {
                    warn!("rotated API key after error: {e}");
                }
                let (attempt, delay) = retry.next_delay();
                if matches!(e, AgentError::Timeout { .. }) && attempt > MAX_TIMEOUT_RETRIES {
                    return Err(e.into());
                }
                let delay_ms = delay.as_millis() as u64;
                warn!(attempt, delay_ms, error = %e, "retryable, will retry");
                if let Some(event_tx) = event_tx {
                    event_tx.send(AgentEvent::Retry {
                        attempt,
                        message: e.retry_message(),
                        delay_ms,
                    })?;
                }
                futures_lite::future::race(
                    async {
                        smol::Timer::after(delay).await;
                    },
                    cancel.cancelled(),
                )
                .await;
                if cancel.is_cancelled() {
                    return Err(StreamError::Cancelled {
                        streamed: String::new(),
                    });
                }
            }
            Err(e) => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                return Err(e.into());
            }
        }
    }
}

fn emit_api_request(model: &Model, r: &StreamResponse, opts: RequestOptions, took: Duration) {
    if !caudra_otel::enabled() {
        return;
    }
    let usage = &r.usage;
    caudra_otel::emit::api_request(&caudra_otel::emit::ApiRequest {
        model: &model.id,
        provider: &model.provider,
        input_tokens: u64::from(usage.input),
        output_tokens: u64::from(usage.output),
        cache_read_tokens: u64::from(usage.cache_read),
        cache_creation_tokens: u64::from(usage.cache_creation),
        cost_usd: model.billed_cost(usage, opts.fast).unwrap_or(0.0),
        duration: took,
        stop_reason: r.stop_reason.map(<&'static str>::from),
    });
}

fn emit_api_error(model: &Model, error: &AgentError, attempt: u32, took: Duration) {
    if !caudra_otel::enabled() {
        return;
    }
    caudra_otel::emit::api_error(&caudra_otel::emit::ApiError {
        model: &model.id,
        provider: &model.provider,
        error: &error_description(error),
        status_code: match error {
            AgentError::Api { status, .. } => Some(*status),
            _ => None,
        },
        attempt,
        duration: took,
    });
}

/// A provider's error body is often echoed request content (quoted message
/// text, masked keys, whatever a gateway returns), so only the status is
/// reported. Every other variant is generated locally.
fn error_description(error: &AgentError) -> String {
    match error {
        AgentError::Api { status, .. } => format!("API error ({status})"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::Role;
    use serde_json::json;

    use super::*;

    const SECRET_BODY: &str = "messages.0.content: \"my private prompt\", key sk-abc";

    #[test]
    fn tool_use_names_canonicalized() {
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::tool_use("t1", "functions.bash", json!({})),
                ContentBlock::tool_use("t2", "read", json!({})),
                ContentBlock::tool_use("t3", "my_functions.x", json!({})),
            ],
            ..Default::default()
        };
        canonicalize_tool_names(&mut message);
        let names: Vec<&str> = message.tool_uses().map(|(_, name, _)| name).collect();
        assert_eq!(names, ["bash", "read", "my_functions.x"]);
    }

    fn thinking_durations(message: &Message) -> Vec<Option<u64>> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect()
    }

    fn thinking_message(blocks: usize) -> Message {
        Message {
            role: Role::Assistant,
            content: (0..blocks)
                .map(|_| ContentBlock::thinking(String::new(), None))
                .chain([ContentBlock::Text { text: "hi".into() }])
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn reasoning_durations_pair_with_thinking_blocks_in_order() {
        let mut message = thinking_message(2);
        attach_reasoning_durations(
            &mut message,
            &[Duration::from_millis(9_700), Duration::from_millis(1_800)],
        );
        assert_eq!(thinking_durations(&message), [Some(9_700), Some(1_800)]);
    }

    #[test]
    fn surplus_thinking_blocks_stay_untimed() {
        let mut message = thinking_message(3);
        attach_reasoning_durations(&mut message, &[Duration::from_millis(42)]);
        assert_eq!(thinking_durations(&message), [Some(42), None, None]);
    }

    #[test]
    fn surplus_durations_are_dropped() {
        let mut message = thinking_message(1);
        attach_reasoning_durations(
            &mut message,
            &[Duration::from_millis(42), Duration::from_millis(7)],
        );
        assert_eq!(thinking_durations(&message), [Some(42)]);
    }

    #[test]
    fn thinking_runs_are_timed_separately_and_flushed_at_stream_end() {
        let (tx, rx) = flume::unbounded();
        for event in [
            ProviderEvent::ThinkingDelta { text: "a".into() },
            ProviderEvent::ThinkingDelta { text: "b".into() },
            ProviderEvent::TextDelta { text: "x".into() },
            ProviderEvent::ThinkingDelta { text: "c".into() },
        ] {
            tx.send(event).unwrap();
        }
        drop(tx);

        let forwarded = smol::block_on(forward_provider_events(rx, None));
        assert_eq!(forwarded.streamed, "x");
        assert_eq!(forwarded.reasoning.len(), 2);
    }

    #[test]
    fn a_stream_without_reasoning_reports_no_durations() {
        let (tx, rx) = flume::unbounded();
        tx.send(ProviderEvent::TextDelta { text: "x".into() })
            .unwrap();
        drop(tx);

        let forwarded = smol::block_on(forward_provider_events(rx, None));
        assert!(forwarded.reasoning.is_empty());
    }

    #[test]
    fn a_reported_api_error_leaves_the_provider_body_behind() {
        let error = AgentError::Api {
            status: 400,
            message: SECRET_BODY.into(),
        };
        let reported = error_description(&error);
        assert!(!reported.contains("private"));
        assert_eq!(reported, "API error (400)");
    }
}
