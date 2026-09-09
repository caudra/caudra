use std::collections::HashMap;
use std::time::{Duration, Instant};

use caudra_providers::provider::Provider;
use caudra_providers::retry::{MAX_RETRIES, RetryDecision, RetryState};
use caudra_providers::{
    Billing, ContentBlock, Message, Model, ProviderEvent, ReasoningSource, RequestOptions,
    StreamResponse,
};
use caudra_storage::id::SessionRef;
use caudra_storage::log::target;
use serde_json::Value;
use tracing::{info, warn};

use super::tool_body::BodyStream;
use super::tool_preview;
use super::tool_roster::RosterStream;
use crate::cancel::CancelToken;
use crate::nudge::Nudge;
use crate::tools::native::batch;
use crate::types::BatchToolEntry;
use crate::{AgentError, AgentEvent, EventSender};

const FUNCTIONS_PREFIX: &str = "functions.";

const EVENT_RETRY: &str = "provider_retry";
const EVENT_RETRY_EXHAUSTED: &str = "provider_retry_exhausted";
const EVENT_RETRY_RECOVERED: &str = "provider_retry_recovered";
const EVENT_KEY_ROTATED: &str = "provider_key_rotated";
const EVENT_REQUEST_FAILED: &str = "provider_request_failed";
const EVENT_REQUEST_FINISHED: &str = "provider_request_finished";
const OUTCOME_OK: &str = "ok";
const OUTCOME_ERROR: &str = "error";
const OUTCOME_CANCELLED: &str = "cancelled";

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
    body: Option<String>,
    roster: Option<Vec<BatchToolEntry>>,
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
    /// Present only for the tools whose argument is a file body, which are the
    /// only ones with a size worth narrating.
    body: Option<BodyStream>,
    /// The last size published, so a body does not repaint the row per token.
    size: Option<String>,
    /// Present only for `batch`, whose whole argument is a list of other calls.
    roster: Option<RosterStream>,
}

impl PendingInput {
    fn new(name: String) -> Self {
        Self {
            body: BodyStream::new(&name),
            roster: RosterStream::new(&name),
            name,
            json: String::new(),
            preview: None,
            settled: false,
            size: None,
        }
    }

    /// The headline settles long before a file body finishes arriving, so the
    /// body reader keeps taking fragments the preview has stopped caring about.
    fn absorb(&mut self, delta: &str) -> Changed {
        // A batch has no headline of its own to scan for: its children are the
        // headline, and the count they add up to is what the row says.
        if self.roster.is_some() {
            let Some(entries) = self.roster.as_mut().and_then(|r| r.absorb(delta)) else {
                return Changed::default();
            };
            return Changed {
                preview: self.published(batch::roster_header(entries.len())),
                roster: Some(entries),
                ..Changed::default()
            };
        }
        let preview = self.absorb_preview(delta);
        let Some(stream) = self.body.as_mut() else {
            return Changed {
                preview,
                ..Changed::default()
            };
        };
        let body = stream.absorb(delta);
        let label = tool_preview::size_label(stream.lines());
        // A call earns its header one way or the other: a patch is named by
        // the envelope its body reader decodes, everything else by the prefix
        // of its arguments.
        let named = stream.header();
        let size = label.filter(|label| self.size.as_ref() != Some(label));
        if let Some(size) = &size {
            self.size = Some(size.clone());
        }
        Changed {
            preview: named.and_then(|header| self.published(header)).or(preview),
            size,
            body,
            roster: None,
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
        self.published(preview.text)
    }

    /// `Some` only for a header that is not the one already on screen, so a
    /// fragment costs a repaint only when it changed the row.
    fn published(&mut self, header: String) -> Option<String> {
        (self.preview.as_deref() != Some(header.as_str())).then(|| {
            self.preview = Some(header.clone());
            header
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
                    body: changed.body,
                    roster: changed.roster,
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
    retry_now: &Nudge,
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
        retry_now,
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
        provider,
        messages,
        model,
        system,
        tools,
        None,
        cancel,
        &Nudge::default(),
        opts,
        session_id,
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
    retry_now: &Nudge,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, StreamError> {
    let opts = opts.clamped(model);
    let messages = caudra_providers::adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut retry = RetryState::new();
    // `started` restarts per attempt, so total time across a retry storm needs
    // its own clock.
    let first_attempt_at = Instant::now();
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
                if retry.attempts() > 0 {
                    info!(
                        target: target::PROVIDER,
                        event = EVENT_RETRY_RECOVERED,
                        provider = %model.provider,
                        model = %model.id,
                        attempt = retry.attempts(),
                        elapsed_ms = first_attempt_at.elapsed().as_millis() as u64,
                        outcome = OUTCOME_OK,
                        "request succeeded after retrying"
                    );
                }
                return Ok(r);
            }
            Err(AgentError::Cancelled) => {
                info!(
                    target: target::PROVIDER,
                    event = EVENT_REQUEST_FINISHED,
                    provider = %model.provider,
                    model = %model.id,
                    attempt = retry.attempts() + 1,
                    duration_ms = started.elapsed().as_millis() as u64,
                    outcome = OUTCOME_CANCELLED,
                    "request cancelled"
                );
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
                    warn!(
                        target: target::PROVIDER,
                        event = EVENT_KEY_ROTATED,
                        provider = %model.provider,
                        error_kind = e.kind(),
                        status = e.status(),
                        "rotated API key after error"
                    );
                }
                let hint_ms = e.retry_after().map(|after| after.as_millis() as u64);
                let (attempt, delay, source) = match retry.decide(e.retry_after()) {
                    RetryDecision::Wait {
                        attempt,
                        delay,
                        source,
                    } => (attempt, delay, source),
                    RetryDecision::GiveUp(reason) => {
                        warn!(
                            target: target::PROVIDER,
                            event = EVENT_RETRY_EXHAUSTED,
                            provider = %model.provider,
                            model = %model.id,
                            attempt = retry.attempts(),
                            max_retries = MAX_RETRIES,
                            reason = reason.as_str(),
                            retry_after_ms = hint_ms,
                            status = e.status(),
                            error_kind = e.kind(),
                            elapsed_ms = first_attempt_at.elapsed().as_millis() as u64,
                            outcome = OUTCOME_ERROR,
                            "giving up after retrying"
                        );
                        return Err(e.into());
                    }
                };
                let delay_ms = delay.as_millis() as u64;
                warn!(
                    target: target::PROVIDER,
                    event = EVENT_RETRY,
                    provider = %model.provider,
                    model = %model.id,
                    attempt,
                    max_retries = MAX_RETRIES,
                    delay_ms,
                    delay_source = source.as_str(),
                    retry_after_ms = hint_ms,
                    status = e.status(),
                    error_kind = e.kind(),
                    error = %e,
                    "retryable, will retry"
                );
                if let Some(event_tx) = event_tx {
                    event_tx.send(AgentEvent::Retry {
                        attempt,
                        message: e.retry_message(),
                        delay_ms,
                    })?;
                }
                let waited = async {
                    futures_lite::future::race(
                        async {
                            smol::Timer::after(delay).await;
                        },
                        retry_now.notified(),
                    )
                    .await;
                };
                futures_lite::future::race(waited, cancel.cancelled()).await;
                if cancel.is_cancelled() {
                    return Err(StreamError::Cancelled {
                        streamed: String::new(),
                        reasoning: Vec::new(),
                    });
                }
            }
            Err(e) => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                // The status is reported but never the body: see
                // `error_description`.
                warn!(
                    target: target::PROVIDER,
                    event = EVENT_REQUEST_FAILED,
                    provider = %model.provider,
                    model = %model.id,
                    attempt = retry.attempts() + 1,
                    status = e.status(),
                    error_kind = e.kind(),
                    auth = e.is_auth_error(),
                    duration_ms = started.elapsed().as_millis() as u64,
                    outcome = OUTCOME_ERROR,
                    "request failed without a retry"
                );
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

/// No telemetry gate: the event feeds the log file through the same call, and
/// the telemetry layer no-ops when telemetry is off.
fn emit_api_request(model: &Model, r: &StreamResponse, opts: RequestOptions, took: Duration) {
    let usage = &r.usage;
    // A subscription's rates describe a bill that never arrives, so the spend
    // metric must not see them.
    let (cost, subscription) = match model.billing {
        Billing::Api => (model.billed_cost(usage, opts.fast), None),
        Billing::Subscription => (None, model.billed_cost(usage, opts.fast)),
    };
    caudra_otel::emit::api_request(&caudra_otel::emit::ApiRequest {
        model: &model.id,
        provider: &model.provider,
        input_tokens: u64::from(usage.input),
        output_tokens: u64::from(usage.output),
        cache_read_tokens: u64::from(usage.cache_read),
        cache_creation_tokens: u64::from(usage.cache_creation),
        cost_usd: cost.unwrap_or(0.0),
        subscription_cost_usd: subscription.unwrap_or(0.0),
        duration: took,
        stop_reason: r.stop_reason.map(<&'static str>::from),
    });
}

fn emit_api_error(model: &Model, error: &AgentError, attempt: u32, took: Duration) {
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
    use test_case::test_case;

    use super::*;

    const SECRET_BODY: &str = "messages.0.content: \"my private prompt\", key sk-abc";
    const TOOL_ID: &str = "toolu_1";

    const WRITE: &str = "file_write";
    const EDIT: &str = "file_edit";
    const PATCH: &str = "file_apply_patch";
    const BATCH: &str = "batch";
    const READ: &str = "file_read";
    const SHELL: &str = "shell";
    /// A body long enough to cross the first step and reach the second.
    const STEPPED_LINES: usize = 9;
    const THRESHOLD_LINES: usize = 4;
    const SHORT_LINES: usize = 3;
    const FIRST_STEP: &str = "5+ lines";
    const SECOND_STEP: &str = "10+ lines";
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

    /// Every roster published, in order, as the tools each row names.
    fn rosters(fragments: &[&str]) -> Vec<Vec<String>> {
        deltas(BATCH, fragments)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolInputDelta { roster, .. } => roster,
                _ => None,
            })
            .map(|entries| entries.into_iter().map(|entry| entry.tool).collect())
            .collect()
    }

    /// The decoded body, reassembled the way a reader accumulates it.
    fn published_body(tool: &str, fragments: &[&str]) -> String {
        deltas(tool, fragments)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolInputDelta { body, .. } => body,
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
            EDIT,
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
        assert!(previews("python_execution", &[r#"{"code": "print(1)"}"#]).is_empty());
    }

    /// A batch used to spend its whole stream as the bare word `Batching`,
    /// then produce every child at once. Its children are its headline, so
    /// they arrive as they are written.
    #[test]
    fn a_batch_publishes_its_children_as_they_are_named() {
        let rosters = rosters(&[
            r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "a.rs"}}"#,
            r#", {"tool": "shell", "parameters": {"command": "ls"}}]}"#,
        ]);
        assert_eq!(
            rosters,
            [
                vec![READ.to_owned()],
                vec![READ.to_owned(), SHELL.to_owned()]
            ]
        );
    }

    /// The count is the header a batch keeps once it runs, so the row it draws
    /// while streaming is the row it settles on.
    #[test]
    fn a_batch_counts_its_children_in_the_header_it_will_keep() {
        let published = previews(
            BATCH,
            &[
                r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "a.rs"}}"#,
                r#", {"tool": "shell", "parameters": {"command": "ls"}}]}"#,
            ],
        );
        assert_eq!(
            published,
            [batch::roster_header(1), batch::roster_header(2)]
        );
    }

    /// Only a batch has a roster, and nothing else may be mistaken for one.
    #[test]
    fn another_tools_arguments_publish_no_roster() {
        let carried = deltas(
            SHELL,
            &[r#"{"command": "ls", "tool_calls": [{"tool": "x"}]}"#],
        )
        .into_iter()
        .any(|event| {
            matches!(
                event,
                AgentEvent::ToolInputDelta {
                    roster: Some(_),
                    ..
                }
            )
        });
        assert!(!carried);
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
        let forwarded: Vec<String> = deltas("python_execution", &fragments)
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
        assert!(sizes("shell", &body(STEPPED_LINES)).is_empty());
    }

    /// An edit is as long a wait as a write, and the header is the only place
    /// that says so before the call runs.
    #[test]
    fn an_edit_is_counted_too() {
        let mut fragments = vec![r#"{"filePath": "a.rs", "oldString": "gone", "newString": "one"#];
        fragments.extend(std::iter::repeat_n(NEWLINE, STEPPED_LINES));
        assert_eq!(sizes(EDIT, &fragments), [FIRST_STEP, SECOND_STEP]);
    }

    /// The case that left the header empty: an ordinary edit swaps a few lines
    /// for a few lines, and neither side reaches the floor alone. The old side
    /// also arrives first, so counting only the new one reports nothing until
    /// the stream is nearly over.
    #[test]
    fn an_ordinary_edit_is_counted_across_both_sides() {
        let fragments = [
            r#"{"filePath": "a.rs", "oldString": "one\ntwo\nthree""#,
            r#", "newString": "1\n2\n3"}"#,
        ];
        assert_eq!(sizes(EDIT, &fragments), [FIRST_STEP]);
    }

    /// A patch is named while it streams the way a write is, except that its
    /// files come out of the envelope rather than an argument of their own.
    #[test]
    fn a_patch_names_its_files_as_they_arrive() {
        let fragments = [
            r#"{"patchText": "*** Begin Patch\n*** Update File: a.rs\n"#,
            r"-one\n+two\n",
            r#"*** Delete File: b.rs\n*** End Patch"}"#,
        ];
        assert_eq!(previews(PATCH, &fragments), ["a.rs", "a.rs, b.rs"]);
    }

    #[test]
    fn a_body_is_published_as_it_is_decoded() {
        let fragments = [r#"{"filePath": "a.rs", "content": "fn x"#, r#"() {}"}"#];
        assert_eq!(published_body(WRITE, &fragments), "fn x() {}");
    }

    #[test]
    fn a_tool_with_no_body_publishes_none() {
        assert!(published_body("shell", &[r#"{"command": "ls"}"#]).is_empty());
    }

    /// A half-written diff is worse than the line count beside it, so these
    /// two are counted into the header and nothing else.
    #[test_case(EDIT, r#"{"filePath": "a.rs", "oldString": "one", "newString": "two"#
        ; "an_edit_publishes_no_body")]
    #[test_case(PATCH, r#"{"patchText": "*** Update File: a.rs\n-one\n+two"#
        ; "a_patch_publishes_no_body")]
    fn a_body_that_is_only_a_diff_is_counted_but_never_published(tool: &str, opening: &str) {
        let mut fragments = vec![opening];
        fragments.extend(std::iter::repeat_n(NEWLINE, STEPPED_LINES));
        assert!(published_body(tool, &fragments).is_empty());
        assert_eq!(sizes(tool, &fragments), [FIRST_STEP, SECOND_STEP]);
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
        let error = AgentError::api(400, SECRET_BODY);
        let reported = error_description(&error);
        assert!(!reported.contains("private"));
        assert_eq!(reported, "API error (400)");
    }
}
