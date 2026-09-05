use std::collections::HashMap;
use std::time::{Duration, Instant};

use caudra_providers::provider::Provider;
use caudra_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use caudra_providers::{
    ContentBlock, Message, Model, ProviderEvent, ReasoningSource, RequestOptions, StreamResponse,
};
use caudra_storage::id::SessionRef;
use serde_json::Value;
use tracing::warn;

use super::tool_preview;
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
    reasoning: Vec<ForwardedReasoning>,
    forwarded: bool,
}

/// What one fragment changed on screen, if anything.
#[derive(Default)]
struct Changed {
    preview: Option<String>,
    size: Option<String>,
}

/// The argument JSON of one tool call as it arrives, kept only until the
/// preview it feeds can no longer change.
struct PendingInput {
    name: String,
    json: String,
    preview: Option<String>,
    /// The preview is final: either its value closed or the buffer outgrew
    /// what is worth scanning. Nothing more is parsed after this.
    settled: bool,
    /// Present only for the tools whose argument is a file body, so nothing is
    /// counted that will not be shown.
    size: Option<SizeCounter>,
}

impl PendingInput {
    fn new(name: String) -> Self {
        Self {
            size: tool_preview::counts_size(&name).then(SizeCounter::default),
            name,
            json: String::new(),
            preview: None,
            settled: false,
        }
    }

    /// The headline settles long before a file body finishes arriving, so the
    /// counter keeps reading fragments the preview has stopped caring about.
    fn absorb(&mut self, delta: &str) -> Changed {
        Changed {
            preview: self.absorb_preview(delta),
            size: self.size.as_mut().and_then(|size| size.absorb(delta)),
        }
    }

    fn absorb_preview(&mut self, delta: &str) -> Option<String> {
        if self.settled {
            return None;
        }
        self.json.push_str(delta);
        let Some(preview) = tool_preview::preview_for(&self.name, &self.json) else {
            self.settled = tool_preview::past_scan_cap(self.json.len());
            return None;
        };
        self.settled = preview.complete;
        if self.settled {
            self.json = String::new();
        }
        let changed = self.preview.as_deref() != Some(preview.text.as_str());
        changed.then(|| {
            self.preview = Some(preview.text.clone());
            preview.text
        })
    }
}

/// Counts the newlines in a body as its fragments go past, retaining none of
/// it. A content newline is the escape `\n`, but `\\n` is an escaped backslash
/// followed by a literal `n` and is not one, so the escape state has to be
/// tracked rather than the two characters counted.
#[derive(Default)]
struct SizeCounter {
    newlines: usize,
    /// An escape can straddle two fragments, so a fragment ending on a lone
    /// backslash carries it to the next.
    escaped: bool,
    shown: Option<String>,
}

impl SizeCounter {
    fn absorb(&mut self, delta: &str) -> Option<String> {
        for c in delta.chars() {
            if self.escaped {
                self.newlines += usize::from(c == 'n');
                self.escaped = false;
            } else {
                self.escaped = c == '\\';
            }
        }
        let label = tool_preview::size_label(self.newlines)?;
        (self.shown.as_deref() != Some(label.as_str())).then(|| {
            self.shown = Some(label.clone());
            label
        })
    }
}

#[derive(Debug)]
pub(crate) struct ForwardedReasoning {
    pub(crate) text: String,
    pub(crate) duration: Duration,
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
    let mut forwarded = false;
    let mut reasoning_text = String::new();
    let mut run_started: Option<Instant> = None;
    let mut pending_inputs: HashMap<String, PendingInput> = HashMap::new();
    while let Ok(pe) = prx.recv_async().await {
        if let ProviderEvent::ThinkingDelta { text } = &pe {
            run_started.get_or_insert_with(Instant::now);
            reasoning_text.push_str(text);
        } else if let Some(started) = run_started.take() {
            reasoning.push(ForwardedReasoning {
                text: std::mem::take(&mut reasoning_text),
                duration: started.elapsed(),
            });
        }
        let ae = match pe {
            ProviderEvent::TextDelta { text } => {
                streamed.push_str(&text);
                AgentEvent::TextDelta { text }
            }
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ThinkingBoundary => AgentEvent::ThinkingBoundary,
            ProviderEvent::ToolUseStart { id, name } => {
                let name = canonical_tool_name(&name).to_owned();
                pending_inputs.insert(id.clone(), PendingInput::new(name.clone()));
                AgentEvent::ToolPending { id, name }
            }
            ProviderEvent::ToolInputDelta { id, delta } => {
                // A provider can emit a fragment before it has a name to
                // announce, leaving nothing to preview and nothing to attach
                // the preview to.
                let Some(pending) = pending_inputs.get_mut(&id) else {
                    continue;
                };
                let changed = pending.absorb(&delta);
                AgentEvent::ToolInputDelta {
                    id,
                    name: pending.name.clone(),
                    delta,
                    preview: changed.preview,
                    size: changed.size,
                }
            }
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
        if let Some(event_tx) = event_tx {
            if event_tx.send(ae).is_err() {
                break;
            }
            forwarded = true;
        }
    }
    if let Some(started) = run_started {
        reasoning.push(ForwardedReasoning {
            text: reasoning_text,
            duration: started.elapsed(),
        });
    }
    ForwardedStream {
        streamed,
        reasoning,
        forwarded,
    }
}

/// Providers append visible thinking blocks in the order their deltas arrive.
/// Opaque blocks have no deltas, so they must not consume a timed run.
fn attach_reasoning_durations(message: &mut Message, durations: &[Duration]) {
    let blocks = message
        .content
        .iter_mut()
        .filter_map(|block| match block {
            ContentBlock::Thinking {
                thinking,
                duration_ms,
                ..
            } if !thinking.is_empty() => Some(duration_ms),
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
    Cancelled {
        streamed: String,
        reasoning: Vec<ForwardedReasoning>,
    },
    Auth {
        error: AgentError,
        forwarded: bool,
    },
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
            StreamError::Auth { error, .. } => error,
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
            forwarded,
        } = forwarder.await;
        match result {
            Ok(mut r) => {
                canonicalize_tool_names(&mut r.message);
                let durations: Vec<_> = reasoning.iter().map(|run| run.duration).collect();
                attach_reasoning_durations(&mut r.message, &durations);
                r.message.reasoning_source = Some(ReasoningSource::new(
                    model,
                    provider.reasoning_transport(model),
                ));
                emit_api_request(model, &r, opts, started.elapsed());
                return Ok(r);
            }
            Err(AgentError::Cancelled) => {
                return Err(StreamError::Cancelled {
                    streamed,
                    reasoning,
                });
            }
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
                        reasoning: Vec::new(),
                    });
                }
            }
            Err(e) => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                if e.is_auth_error() {
                    return Err(StreamError::Auth {
                        error: e,
                        forwarded,
                    });
                }
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
    const TOOL_ID: &str = "toolu_1";

    const WRITE: &str = "file_write";
    /// A body long enough to cross the first step and reach the second.
    const STEPPED_LINES: usize = 29;
    const THRESHOLD_LINES: usize = 19;
    const SHORT_LINES: usize = 18;
    const FIRST_STEP: &str = "20+ lines";
    const SECOND_STEP: &str = "30+ lines";
    /// The escape a body newline arrives as, and the pair that only looks like
    /// one: a backslash that is itself escaped, then a literal `n`.
    const NEWLINE: &str = r"\n";
    const ESCAPED_BACKSLASH: &str = r"\\n";

    /// Every tool-input event a run of provider events publishes, in order.
    fn deltas(tool: &str, fragments: &[&str]) -> Vec<AgentEvent> {
        let (ptx, prx) = flume::unbounded();
        ptx.send(ProviderEvent::ToolUseStart {
            id: TOOL_ID.into(),
            name: tool.into(),
        })
        .unwrap();
        for fragment in fragments {
            ptx.send(ProviderEvent::ToolInputDelta {
                id: TOOL_ID.into(),
                delta: (*fragment).into(),
            })
            .unwrap();
        }
        drop(ptx);

        let (etx, erx) = flume::unbounded();
        let sender = crate::EventSender::new(etx, 0);
        smol::block_on(forward_provider_events(prx, Some(&sender)));
        drop(sender);
        erx.drain()
            .map(|envelope| envelope.event)
            .filter(|event| matches!(event, AgentEvent::ToolInputDelta { .. }))
            .collect()
    }

    /// Every preview published, in order. `None` fragments are dropped, so the
    /// result is exactly what a reader repaints.
    fn previews(tool: &str, fragments: &[&str]) -> Vec<String> {
        deltas(tool, fragments)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolInputDelta { preview, .. } => preview,
                _ => None,
            })
            .collect()
    }

    fn sizes(tool: &str, fragments: &[&str]) -> Vec<String> {
        deltas(tool, fragments)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolInputDelta { size, .. } => size,
                _ => None,
            })
            .collect()
    }

    /// A named file followed by one body newline per fragment.
    fn body(newlines: usize) -> Vec<&'static str> {
        let mut fragments = vec![r#"{"filePath": "a.rs", "content": "first"#];
        fragments.extend(std::iter::repeat_n(NEWLINE, newlines));
        fragments
    }

    #[test]
    fn a_preview_is_published_only_when_it_changes() {
        let published = previews("shell", &[r#"{"comm"#, r#"and": "ec"#, "ho", r#" hi"}"#]);
        assert_eq!(published, ["ec", "echo", "echo hi"]);
    }

    #[test]
    fn a_settled_preview_ignores_the_rest_of_the_arguments() {
        let published = previews(
            "file_edit",
            &[
                r#"{"filePath": "a.rs""#,
                r#", "oldString": "one""#,
                r#", "newString": "two"}"#,
            ],
        );
        assert_eq!(published, ["a.rs"]);
    }

    #[test]
    fn a_tool_with_nothing_short_to_show_publishes_no_preview() {
        assert!(previews("code_execution", &[r#"{"code": "print(1)"}"#]).is_empty());
    }

    #[test]
    fn a_fragment_for_an_unannounced_call_is_dropped() {
        let (ptx, prx) = flume::unbounded();
        ptx.send(ProviderEvent::ToolInputDelta {
            id: TOOL_ID.into(),
            delta: r#"{"command": "ls"}"#.into(),
        })
        .unwrap();
        drop(ptx);

        let (etx, erx) = flume::unbounded();
        let sender = crate::EventSender::new(etx, 0);
        smol::block_on(forward_provider_events(prx, Some(&sender)));
        drop(sender);
        assert_eq!(erx.drain().count(), 0);
    }

    #[test]
    fn every_fragment_is_forwarded_verbatim_even_without_a_preview() {
        let fragments = [r#"{"code": ""#, "print(1)", r#""}"#];
        let forwarded: Vec<String> = deltas("code_execution", &fragments)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolInputDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(forwarded, fragments);
    }

    #[test]
    fn a_size_is_published_once_per_step() {
        assert_eq!(
            sizes(WRITE, &body(STEPPED_LINES)),
            [FIRST_STEP, SECOND_STEP]
        );
    }

    #[test]
    fn a_body_below_the_threshold_publishes_no_size() {
        assert!(sizes(WRITE, &body(SHORT_LINES)).is_empty());
    }

    #[test]
    fn a_tool_with_no_body_to_count_publishes_no_size() {
        assert!(sizes("file_edit", &body(STEPPED_LINES)).is_empty());
    }

    #[test]
    fn an_escaped_backslash_is_not_a_newline() {
        let mut fragments = body(THRESHOLD_LINES);
        fragments.extend(std::iter::repeat_n(ESCAPED_BACKSLASH, STEPPED_LINES));
        assert_eq!(sizes(WRITE, &fragments), [FIRST_STEP]);
    }

    #[test]
    fn an_escape_split_across_two_fragments_is_still_counted() {
        let mut fragments = vec![r#"{"filePath": "a.rs", "content": "first"#];
        for _ in 0..THRESHOLD_LINES {
            fragments.extend([r"\", "n"]);
        }
        assert_eq!(sizes(WRITE, &fragments), [FIRST_STEP]);
    }

    #[test]
    fn the_headline_settles_while_the_counter_keeps_reading() {
        let fragments = body(STEPPED_LINES);
        assert_eq!(previews(WRITE, &fragments), ["a.rs"]);
        assert_eq!(sizes(WRITE, &fragments), [FIRST_STEP, SECOND_STEP]);
    }

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
                .map(|index| ContentBlock::thinking(format!("reasoning {index}"), None))
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
    fn opaque_thinking_does_not_consume_a_visible_duration() {
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking(String::new(), None),
                ContentBlock::thinking("visible".into(), None),
            ],
            ..Default::default()
        };

        attach_reasoning_durations(&mut message, &[Duration::from_millis(42)]);

        assert_eq!(thinking_durations(&message), [None, Some(42)]);
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
        assert_eq!(forwarded.reasoning[0].text, "ab");
        assert_eq!(forwarded.reasoning[1].text, "c");
    }

    #[test]
    fn provider_reasoning_boundary_splits_streamed_blocks() {
        let (tx, rx) = flume::unbounded();
        for event in [
            ProviderEvent::ThinkingDelta {
                text: "first".into(),
            },
            ProviderEvent::ThinkingBoundary,
            ProviderEvent::ThinkingDelta {
                text: "second".into(),
            },
        ] {
            tx.send(event).unwrap();
        }
        drop(tx);

        let forwarded = smol::block_on(forward_provider_events(rx, None));
        assert_eq!(forwarded.reasoning.len(), 2);
        assert_eq!(forwarded.reasoning[0].text, "first");
        assert_eq!(forwarded.reasoning[1].text, "second");
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
