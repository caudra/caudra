use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_providers::provider::Provider;
use caudra_providers::retry::{MAX_RETRIES, RetryDecision, RetryState};
use caudra_providers::{
    Billing, CacheKey, ContentBlock, MAX_TOOL_INPUT_BYTES, Message, Model, ProviderEvent,
    ReasoningSource, RequestOptions, StopReason, StreamResponse,
};
use caudra_storage::log::target;
use serde_json::Value;
use tracing::{info, warn};

use super::speculative::SpeculativeRuns;
use super::tool_body::BodyStream;
use super::tool_delegation::{Delegated, DelegationStream};
use super::tool_preview;
use super::tool_roster::RosterStream;
use crate::cancel::CancelToken;
use crate::nudge::Nudge;
use crate::tools::native::batch;
use crate::types::{BatchToolEntry, Delegation};
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
    /// One per delegating call the fragment moved: the call itself, or the
    /// `batch` children writing their briefs.
    delegations: Vec<Delegation>,
    /// The `batch` children whose arguments the fragment finished, whole.
    /// Dispatched after the fragment's own event has gone out, so a child's
    /// first progress never arrives before the row it belongs to.
    ready: Vec<(usize, String)>,
}

/// The argument JSON of one tool call as it arrives, kept only until the
/// preview it feeds can no longer change.
struct PendingInput {
    execution: InputFrame,
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
    /// Present only for `task`, whose brief opens a chat before it runs.
    delegation: Option<DelegationStream>,
    /// The call's own id, so a delegation can name the chat its subagent will
    /// publish under.
    id: String,
}

impl PendingInput {
    fn new(id: String, name: String, dispatching: bool) -> Self {
        Self {
            execution: InputFrame::default(),
            body: BodyStream::new(&name),
            roster: RosterStream::new(&name, dispatching),
            delegation: DelegationStream::new(&name),
            name,
            id,
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
            let rostered = self
                .roster
                .as_mut()
                .map(|roster| roster.absorb(delta))
                .unwrap_or_default();
            let delegations = rostered
                .delegated
                .into_iter()
                .map(|(index, delegated)| {
                    delegation(batch::child_tool_use_id(Some(&self.id), index), delegated)
                })
                .collect();
            let ready = rostered.ready;
            let Some(entries) = rostered.entries else {
                return Changed {
                    delegations,
                    ready,
                    ..Changed::default()
                };
            };
            return Changed {
                preview: self.published(batch::roster_header(entries.len())),
                roster: Some(entries),
                delegations,
                ready,
                ..Changed::default()
            };
        }
        let delegations = self
            .delegation
            .as_mut()
            .and_then(|stream| stream.absorb(delta))
            .map(|delegated| vec![delegation(self.id.clone(), delegated)])
            .unwrap_or_default();
        let preview = self.absorb_preview(delta);
        let Some(stream) = self.body.as_mut() else {
            return Changed {
                preview,
                delegations,
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
            delegations,
            ready: Vec::new(),
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
        // A preview built from several arguments need never complete: the last
        // of them can trail a body long enough to outrun the scan. Giving up at
        // the cap is what keeps the buffer from following the body.
        self.settled = preview.complete || tool_preview::past_scan_cap(self.json.len());
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

#[derive(Default)]
struct InputFrame {
    raw: String,
    depth: usize,
    quoted: bool,
    escaped: bool,
    container: bool,
    stopped: bool,
}

impl InputFrame {
    fn absorb(&mut self, delta: &str) -> Option<Value> {
        if self.stopped {
            return None;
        }
        if self.raw.len().saturating_add(delta.len()) > MAX_TOOL_INPUT_BYTES {
            self.stopped = true;
            self.raw.clear();
            return None;
        }
        let mut boundary = false;
        for c in delta.chars() {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if c == '\\' {
                    self.escaped = true;
                } else if c == '"' {
                    self.quoted = false;
                }
            } else {
                match c {
                    '"' => self.quoted = true,
                    '{' | '[' => {
                        self.container = true;
                        self.depth += 1;
                    }
                    '}' | ']' => {
                        self.depth = self.depth.saturating_sub(1);
                        boundary |= self.depth == 0;
                    }
                    _ => {}
                }
            }
        }
        self.raw.push_str(delta);
        if !boundary || !self.container {
            return None;
        }
        let input = serde_json::from_str(&self.raw).ok()?;
        self.stopped = true;
        self.raw.clear();
        Some(input)
    }
}

/// Names a decoded brief with the id the subagent it opens will publish under.
fn delegation(parent_tool_use_id: String, delegated: Delegated) -> Delegation {
    Delegation {
        parent_tool_use_id,
        name: delegated.name,
        prompt: delegated.prompt,
        task_id: delegated.task_id,
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
    progress_only: bool,
    speculative: Option<Arc<SpeculativeRuns>>,
) -> ForwardedStream {
    let mut streamed = String::new();
    let mut reasoning = Vec::new();
    let mut forwarded = false;
    let mut reasoning_text = String::new();
    let mut run_started: Option<Instant> = None;
    let mut pending_inputs: HashMap<String, PendingInput> = HashMap::new();
    let mut aliases = None;
    while let Ok(pe) = prx.recv_async().await {
        if let ProviderEvent::ToolAliases { aliases: current } = pe {
            if let Some(runs) = &speculative {
                runs.set_aliases(current.clone());
            }
            aliases = current;
            continue;
        }
        if let ProviderEvent::ThinkingDelta { text } = &pe {
            run_started.get_or_insert_with(Instant::now);
            reasoning_text.push_str(text);
        } else if matches!(pe, ProviderEvent::PromptProgress { .. }) {
            // A prefill ping is transport, not content. Ending the run on one
            // would split a single thinking block into several, and only the
            // first of those is paired with it, so a minute of reasoning would
            // be recorded as the millisecond before the first ping.
        } else if let Some(started) = run_started.take() {
            reasoning.push(ForwardedReasoning {
                text: std::mem::take(&mut reasoning_text),
                duration: started.elapsed(),
            });
        }
        let forward = !progress_only || matches!(&pe, ProviderEvent::PromptProgress { .. });
        // The call whose children this fragment finished, and the children
        // themselves. Dispatched below, once the row they report on exists.
        let mut ready = (String::new(), Vec::new());
        let mut top_ready = None;
        let ae = match pe {
            ProviderEvent::ToolAliases { .. } => continue,
            ProviderEvent::ToolInputReady {
                id,
                name,
                input,
                invalid_input,
            } => {
                let name = canonical_tool_name(&name);
                let name = aliases
                    .as_ref()
                    .and_then(|aliases| aliases.get(name))
                    .map(String::as_str)
                    .unwrap_or(name)
                    .to_owned();
                if !pending_inputs.contains_key(&id) {
                    if forward && let Some(tx) = event_tx {
                        if tx
                            .send(AgentEvent::ToolPending {
                                id: id.clone(),
                                name: name.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        forwarded = true;
                    }
                    let mut pending = PendingInput::new(id.clone(), name.clone(), false);
                    let delta = input.to_string();
                    let changed = pending.absorb(&delta);
                    if forward
                        && let Some(tx) = event_tx
                        && tx
                            .send(AgentEvent::ToolInputDelta {
                                id: id.clone(),
                                name: name.clone(),
                                delta,
                                preview: changed.preview,
                                size: changed.size,
                                body: changed.body,
                                roster: changed.roster,
                                delegations: changed.delegations,
                            })
                            .is_err()
                    {
                        break;
                    }
                    pending_inputs.insert(id.clone(), pending);
                }
                if let Some(runs) = &speculative {
                    runs.register(&id, &name);
                    if invalid_input.is_none() {
                        runs.ready(&id, &name, input);
                    }
                }
                continue;
            }
            ProviderEvent::TextDelta { text } => {
                streamed.push_str(&text);
                AgentEvent::TextDelta { text }
            }
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ThinkingBoundary => AgentEvent::ThinkingBoundary,
            ProviderEvent::ToolUseStart {
                id,
                name,
                source_ordinal,
            } => {
                let name = canonical_tool_name(&name);
                let name = aliases
                    .as_ref()
                    .and_then(|aliases| aliases.get(name))
                    .map(String::as_str)
                    .unwrap_or(name)
                    .to_owned();
                if pending_inputs.contains_key(&id) {
                    continue;
                }
                if let Some(runs) = &speculative {
                    runs.register_source(&id, &name, source_ordinal);
                }
                pending_inputs.insert(
                    id.clone(),
                    PendingInput::new(id.clone(), name.clone(), speculative.is_some()),
                );
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
                if let Some(input) = pending.execution.absorb(&delta) {
                    top_ready = Some((id.clone(), pending.name.clone(), input));
                }
                ready = (id.clone(), changed.ready);
                AgentEvent::ToolInputDelta {
                    id,
                    name: pending.name.clone(),
                    delta,
                    preview: changed.preview,
                    size: changed.size,
                    body: changed.body,
                    roster: changed.roster,
                    delegations: changed.delegations,
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
        if forward && let Some(event_tx) = event_tx {
            if event_tx.send(ae).is_err() {
                break;
            }
            forwarded = true;
        }
        // After the send, never before it: the row a child reports on is
        // published by this fragment's own event, and the queue is ordered, so
        // an already-sent roster cannot be overtaken by a child the next line
        // starts.
        let (batch_id, children) = ready;
        if let Some(runs) = speculative.as_ref() {
            for (index, element) in children {
                runs.start(&batch_id, index, &element);
            }
            if let Some((id, name, input)) = top_ready {
                runs.ready(&id, &name, input);
            }
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
    Partial {
        response: Box<StreamResponse>,
        error: AgentError,
    },
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
            StreamError::Partial { error, .. } => error,
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
    cache_key: Option<&CacheKey>,
    speculative: Option<&Arc<SpeculativeRuns>>,
) -> Result<StreamResponse, StreamError> {
    stream_with_retry_inner(
        provider,
        messages,
        model,
        system,
        tools,
        Some(event_tx),
        false,
        cancel,
        retry_now,
        opts,
        cache_key,
        speculative,
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
    progress_tx: Option<&EventSender>,
    cancel: &CancelToken,
    opts: RequestOptions,
    cache_key: Option<&CacheKey>,
) -> Result<StreamResponse, StreamError> {
    stream_with_retry_inner(
        provider,
        messages,
        model,
        system,
        tools,
        progress_tx,
        true,
        cancel,
        &Nudge::default(),
        opts,
        cache_key,
        None,
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
    progress_only: bool,
    cancel: &CancelToken,
    retry_now: &Nudge,
    opts: RequestOptions,
    cache_key: Option<&CacheKey>,
    speculative: Option<&Arc<SpeculativeRuns>>,
) -> Result<StreamResponse, StreamError> {
    let opts = opts.clamped(model);
    let messages = caudra_providers::adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut retry = RetryState::new();
    // `started` restarts per attempt, so total time across a retry storm needs
    // its own clock.
    let first_attempt_at = Instant::now();
    loop {
        if let Some(runs) = speculative {
            runs.begin_attempt();
        }
        let started = Instant::now();
        let (ptx, prx) = flume::unbounded();
        let forwarder = smol::spawn({
            let event_tx = event_tx.cloned();
            let speculative = speculative.cloned();
            async move {
                forward_provider_events(prx, event_tx.as_ref(), progress_only, speculative).await
            }
        });
        let result = futures_lite::future::race(
            provider.stream_message(
                model,
                messages,
                system,
                tools,
                &ptx,
                opts.clone(),
                cache_key,
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
                if let Some(runs) = speculative {
                    runs.reconcile(&mut r.message);
                    runs.finalize(&mut r);
                }
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
            Err(e) if speculative.is_some_and(|runs| runs.has_admitted()) => {
                if !matches!(e, AgentError::Cancelled) {
                    emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                }
                let Some(runs) = speculative else {
                    return Err(e.into());
                };
                let mut message = runs.recover_partial();
                let mut content: Vec<_> =
                    reasoning
                        .into_iter()
                        .filter(|run| !run.text.is_empty())
                        .map(|run| ContentBlock::Thinking {
                            thinking: run.text,
                            signature: None,
                            duration_ms: Some(
                                run.duration.as_millis().min(u128::from(u64::MAX)) as u64
                            ),
                            interrupted: true,
                            responses: None,
                        })
                        .collect();
                if !streamed.is_empty() {
                    content.push(ContentBlock::Text { text: streamed });
                }
                content.append(&mut message.content);
                message.content = content;
                message.reasoning_source = Some(ReasoningSource::new(
                    model,
                    provider.reasoning_transport(model),
                ));
                return Err(StreamError::Partial {
                    response: Box::new(StreamResponse {
                        message,
                        stop_reason: Some(StopReason::ToolUse),
                        tool_name_aliases: runs.aliases(),
                        ..StreamResponse::default()
                    }),
                    error: e,
                });
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
                // The attempt's children outlive their ids: the message they
                // were dispatched for is gone, so whatever they went on to
                // report would name a row the retry will never draw. What
                // already answered is kept, and the retried message either
                // claims it or reports it.
                if let Some(runs) = speculative {
                    runs.abandon_unfinished();
                }
                // Listening before the event goes out: a nudge is dropped when
                // nothing is waiting, and announcing the wait first invites one
                // to arrive in the gap before it starts.
                let nudged = retry_now.listen();
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
                        async {
                            nudged.await;
                        },
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
    use crate::agent::tool_dispatch::{ResponseObservations, ToolOutcome};
    use crate::tools::{ToolLive, ToolSource};
    use caudra_providers::Role;
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{InvalidToolInput, ModelInfo, invalid_tool_input};
    use serde_json::json;
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_case::test_case;

    use super::*;

    const SECRET_BODY: &str = "messages.0.content: \"my private prompt\", key sk-abc";
    const TOOL_ID: &str = "toolu_1";
    const INCOMPLETE_ID: &str = "toolu_incomplete";
    const WIRE_READ: &str = "wire_read";
    const LIVE_ANNOTATION: &str = "building graph";
    const TRANSPORT_FAILURE: &str = "stream interrupted";
    const MALFORMED_COMMAND: &str = r#"{"command":"echo safe""#;
    const COMMAND: &str = "echo safe";
    const PARTIAL_REASONING: &str = "Inspect the file first.";
    const PARTIAL_TEXT: &str = "Reading the file.";
    const EARLY_INPUT: &str = r#"{"path":"early"}"#;
    const LATE_INPUT: &str = r#"{"path":"late"}"#;
    const EARLY_FAILURE: &str = "earlier call failed";

    #[test_case(1 ; "bounded_source_prefix")]
    #[test_case(2 ; "distinct_source_slots")]
    fn independent_start_executes_before_tail_without_reordering_observations(limit: usize) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
            let observations = ResponseObservations::new(limit);
            ctx.steering_observations = Some(observations.clone());
            let (executed, effects) = flume::unbounded();
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(move |input, ctx| {
                    let executed = executed.clone();
                    Box::pin(async move {
                        let earlier = ctx.tool_use_id.as_deref() == Some(INCOMPLETE_ID);
                        executed.send((ctx.steering_order, input)).unwrap();
                        if earlier {
                            Err(EARLY_FAILURE.into())
                        } else {
                            Ok(String::new())
                        }
                    })
                }),
            )]));
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            let (tx, rx) = flume::unbounded();
            let forwarding = smol::spawn({
                let runs = Arc::clone(&runs);
                async move { forward_provider_events(rx, None, false, Some(runs)).await }
            });
            tx.send(ProviderEvent::ToolUseStart {
                id: TOOL_ID.into(),
                name: READ.into(),
                source_ordinal: Some(1),
            })
            .unwrap();
            tx.send(ProviderEvent::ToolInputDelta {
                id: TOOL_ID.into(),
                delta: LATE_INPUT.into(),
            })
            .unwrap();
            let later: Value = serde_json::from_str(LATE_INPUT).unwrap();
            assert_eq!(
                effects.recv_async().await.unwrap(),
                (vec![1], later.clone())
            );
            tx.send(ProviderEvent::TextDelta {
                text: PARTIAL_TEXT.into(),
            })
            .unwrap();
            tx.send(ProviderEvent::ToolUseStart {
                id: INCOMPLETE_ID.into(),
                name: READ.into(),
                source_ordinal: Some(0),
            })
            .unwrap();
            for fragment in ["{", &EARLY_INPUT[1..]] {
                tx.send(ProviderEvent::ToolInputDelta {
                    id: INCOMPLETE_ID.into(),
                    delta: fragment.into(),
                })
                .unwrap();
            }
            let earlier: Value = serde_json::from_str(EARLY_INPUT).unwrap();
            assert_eq!(
                effects.recv_async().await.unwrap(),
                (vec![0], earlier.clone())
            );
            for (id, input) in [(INCOMPLETE_ID, &earlier), (TOOL_ID, &later)] {
                tx.send(ProviderEvent::ToolInputReady {
                    id: id.into(),
                    name: READ.into(),
                    input: input.clone(),
                    invalid_input: None,
                })
                .unwrap();
            }
            drop(tx);
            forwarding.await;
            let mut response = StreamResponse {
                message: Message {
                    content: vec![
                        ContentBlock::tool_use(INCOMPLETE_ID, READ, earlier.clone()),
                        ContentBlock::tool_use(TOOL_ID, READ, later.clone()),
                    ],
                    ..Message::default()
                },
                ..StreamResponse::default()
            };
            runs.finalize(&mut response);
            runs.settled().await;
            assert!(effects.is_empty());
            assert_eq!(
                runs.partial_message()
                    .tool_uses()
                    .map(|(id, _, _)| id)
                    .collect::<Vec<_>>(),
                [INCOMPLETE_ID, TOOL_ID]
            );
            let first = runs
                .claim(INCOMPLETE_ID, READ, &earlier)
                .unwrap()
                .finish()
                .await;
            assert!(first.is_error);
            assert_eq!(first.output.as_text(), EARLY_FAILURE);
            assert!(
                !runs
                    .claim(TOOL_ID, READ, &later)
                    .unwrap()
                    .finish()
                    .await
                    .is_error
            );
            let (facts, _) = observations.take();
            assert_eq!(facts.len(), limit);
            assert_eq!(facts[0].outcome, ToolOutcome::Failure);
            if limit > 1 {
                assert_eq!(facts[1].outcome, ToolOutcome::Success);
                assert_ne!(facts[0].fingerprint, facts[1].fingerprint);
            }
        });
    }

    #[test_case(false, false ; "max_tokens")]
    #[test_case(true, false ; "completed_response")]
    #[test_case(true, true ; "final_revision")]
    fn malformed_top_level_waits_for_final_response(complete: bool, revised: bool) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
            let (executed, effects) = flume::unbounded();
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(move |input, _| {
                    let executed = executed.clone();
                    Box::pin(async move {
                        executed.send(input).unwrap();
                        Ok(String::new())
                    })
                }),
            )]));
            ctx.json_repair.register_definitions(&json!([{"name":READ,"input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]));
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            let (tx, rx) = flume::unbounded();
            let marker = invalid_tool_input(MALFORMED_COMMAND);
            for event in [
                ProviderEvent::ToolUseStart {
                    id: TOOL_ID.into(),
                    name: READ.into(),
                    source_ordinal: None,
                },
                ProviderEvent::ToolInputDelta {
                    id: TOOL_ID.into(),
                    delta: MALFORMED_COMMAND.into(),
                },
                ProviderEvent::ToolInputReady {
                    id: TOOL_ID.into(),
                    name: READ.into(),
                    input: marker.clone(),
                    invalid_input: Some(InvalidToolInput {
                        raw: MALFORMED_COMMAND.into(),
                        complete: true,
                        clipped: false,
                    }),
                },
            ] {
                tx.send(event).unwrap();
            }
            drop(tx);
            forward_provider_events(rx, None, false, Some(Arc::clone(&runs))).await;
            assert!(!runs.has_admitted());
            assert!(effects.is_empty());
            assert!(ctx.json_repair.invalid_input(TOOL_ID).is_none());
            let input = if revised {
                json!({"command":COMMAND})
            } else {
                marker
            };
            let mut response = StreamResponse {
                message: Message {
                    content: vec![ContentBlock::tool_use(TOOL_ID, READ, input.clone())],
                    ..Message::default()
                },
                stop_reason: Some(if complete {
                    StopReason::ToolUse
                } else {
                    StopReason::MaxTokens
                }),
                ..StreamResponse::default()
            };
            if !revised {
                response.invalid_tool_inputs.insert(
                    TOOL_ID.into(),
                    InvalidToolInput {
                        raw: MALFORMED_COMMAND.into(),
                        complete,
                        clipped: false,
                    },
                );
            }
            runs.finalize(&mut response);
            runs.settled().await;
            let done = runs.claim(TOOL_ID, READ, &input).unwrap().finish().await;
            assert_eq!(done.is_error, !complete);
            if complete {
                assert_eq!(effects.try_recv().unwrap(), json!({"command":COMMAND}));
            }
            runs.finalize(&mut response);
            runs.settled().await;
            assert!(effects.is_empty());
        });
    }

    struct AdmissionFailureProvider {
        calls: AtomicUsize,
        batch: bool,
        status: u16,
        wait_for_cancel: bool,
    }

    impl Provider for AdmissionFailureProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            events: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Ok(StreamResponse::default());
                }
                events
                    .send(ProviderEvent::ThinkingDelta {
                        text: PARTIAL_REASONING.into(),
                    })
                    .unwrap();
                events
                    .send(ProviderEvent::TextDelta {
                        text: PARTIAL_TEXT.into(),
                    })
                    .unwrap();
                events
                    .send(ProviderEvent::ToolUseStart {
                        id: TOOL_ID.into(),
                        name: if self.batch { BATCH } else { READ }.into(),
                        source_ordinal: None,
                    })
                    .unwrap();
                let delta = if self.batch {
                    format!("{{\"tool_calls\":[{{\"tool\":\"{READ}\",\"parameters\":{{}}}},")
                } else {
                    "{}".into()
                };
                events
                    .send(ProviderEvent::ToolInputDelta {
                        id: TOOL_ID.into(),
                        delta,
                    })
                    .unwrap();
                if self.wait_for_cancel {
                    return pending().await;
                }
                Err(AgentError::api(self.status, TRANSPORT_FAILURE))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test_case(false, 503, false ; "top_level_transport")]
    #[test_case(true, 503, false ; "unfinished_batch_transport")]
    #[test_case(false, 401, false ; "admitted_auth_failure")]
    #[test_case(false, 503, true ; "top_level_user_cancellation")]
    #[test_case(true, 503, true ; "unfinished_batch_user_cancellation")]
    fn admitted_calls_settle_without_transport_replay(batch: bool, status: u16, cancelled: bool) {
        smol::block_on(async {
            let (events, _rx) = flume::unbounded();
            let sender = EventSender::new(events, 0);
            let mut ctx = crate::tools::test_support::stub_ctx_with(
                &crate::AgentMode::Build,
                Some(&sender),
                None,
            );
            ctx.registry
                .register(
                    Arc::new(batch::BatchTool),
                    ToolSource::Native {
                        owner: crate::tools::native::OWNER.into(),
                        contract: BATCH.into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let executed = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&executed);
            let (effect, effects) = flume::unbounded();
            let (cancel, token) = CancelToken::new();
            let mut cancel = Some(cancel);
            ctx.cancel = token;
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(move |_, _| {
                    let count = Arc::clone(&count);
                    let effect = effect.clone();
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        effect.send(()).unwrap();
                        if cancelled {
                            return pending().await;
                        }
                        Ok(String::new())
                    })
                }),
            )]));
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            let provider = AdmissionFailureProvider {
                calls: AtomicUsize::new(0),
                batch,
                status,
                wait_for_cancel: cancelled,
            };
            let tools = json!([]);
            let nudge = Nudge::default();
            let request = stream_with_retry(
                &provider,
                &ctx.model,
                &[],
                "",
                &tools,
                &sender,
                &ctx.cancel,
                &nudge,
                RequestOptions::default(),
                None,
                Some(&runs),
            );
            let result = futures_lite::future::race(request, async {
                if cancelled {
                    effects.recv_async().await.unwrap();
                    cancel.take().unwrap().cancel();
                }
                pending().await
            })
            .await;
            let Err(StreamError::Partial {
                mut response,
                error,
            }) = result
            else {
                panic!("expected an admitted partial response");
            };
            runs.settled().await;
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(executed.load(Ordering::SeqCst), 1);
            assert_eq!(matches!(error, AgentError::Cancelled), cancelled);
            assert!(
                matches!(&response.message.content[0], ContentBlock::Thinking {
                thinking,
                interrupted: true,
                ..
            } if thinking == PARTIAL_REASONING)
            );
            assert_eq!(response.message.first_text_content(), Some(PARTIAL_TEXT));
            assert!(response.message.reasoning_source.is_some());
            let uses: Vec<_> = response.message.tool_uses().collect();
            assert_eq!(uses.len(), 1);
            let (id, name, input) = uses[0];
            let done = runs.claim(id, name, input).unwrap().finish().await;
            assert_eq!(done.is_error, cancelled);
            assert_eq!(done.id, TOOL_ID);
            runs.finalize(&mut response);
            runs.settled().await;
            assert_eq!(executed.load(Ordering::SeqCst), 1);
        });
    }

    #[test_case(false ; "streamed_json")]
    #[test_case(true ; "done_only")]
    fn top_level_call_executes_while_response_channel_remains_open(done_only: bool) {
        smol::block_on(async {
            let (etx, erx) = flume::unbounded();
            let sender = EventSender::new(etx, 0);
            let mut ctx = crate::tools::test_support::stub_ctx_with(
                &crate::AgentMode::Build,
                Some(&sender),
                None,
            );
            let (executed, effects) = flume::unbounded();
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(move |_, ctx| {
                    let executed = executed.clone();
                    Box::pin(async move {
                        ctx.live_sink
                            .as_ref()
                            .unwrap()
                            .send(ToolLive::Annotation(LIVE_ANNOTATION.into()))
                            .unwrap();
                        executed.send(()).unwrap();
                        Ok(String::new())
                    })
                }),
            )]));
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            let (ptx, prx) = flume::unbounded();
            let forwarding = smol::spawn({
                let runs = Arc::clone(&runs);
                let sender = sender.clone();
                async move { forward_provider_events(prx, Some(&sender), false, Some(runs)).await }
            });
            ptx.send(ProviderEvent::ToolAliases {
                aliases: Some(Arc::new(HashMap::from([(WIRE_READ.into(), READ.into())]))),
            })
            .unwrap();
            let input = json!({"path": "a.rs"});
            ptx.send(ProviderEvent::ToolUseStart {
                id: INCOMPLETE_ID.into(),
                name: WIRE_READ.into(),
                source_ordinal: None,
            })
            .unwrap();
            ptx.send(ProviderEvent::ToolInputDelta {
                id: INCOMPLETE_ID.into(),
                delta: "{".into(),
            })
            .unwrap();
            if done_only {
                ptx.send(ProviderEvent::ToolInputReady {
                    id: TOOL_ID.into(),
                    name: WIRE_READ.into(),
                    input: input.clone(),
                    invalid_input: None,
                })
                .unwrap();
            } else {
                ptx.send(ProviderEvent::ToolUseStart {
                    id: TOOL_ID.into(),
                    name: WIRE_READ.into(),
                    source_ordinal: None,
                })
                .unwrap();
                ptx.send(ProviderEvent::ToolInputDelta {
                    id: TOOL_ID.into(),
                    delta: input.to_string(),
                })
                .unwrap();
            }
            effects.recv_async().await.unwrap();
            ptx.send(ProviderEvent::ToolInputReady {
                id: TOOL_ID.into(),
                name: WIRE_READ.into(),
                input: input.clone(),
                invalid_input: None,
            })
            .unwrap();
            ptx.send(ProviderEvent::TextDelta {
                text: String::new(),
            })
            .unwrap();
            drop(ptx);
            forwarding.await;
            runs.settled().await;
            assert!(effects.is_empty());
            assert!(
                !runs
                    .claim(TOOL_ID, READ, &input)
                    .unwrap()
                    .finish()
                    .await
                    .is_error
            );
            let events: Vec<_> = erx.drain().map(|event| event.event).collect();
            let preview = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolInputDelta { .. }))
                .unwrap();
            let start = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolStart(_)))
                .unwrap();
            assert!(preview < start);
            let live = events.iter().position(|event| matches!(event, AgentEvent::ToolAnnotation { id, annotation } if id == TOOL_ID && annotation == LIVE_ANNOTATION)).unwrap();
            let done = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolDone(_)))
                .unwrap();
            assert!(start < live && live < done);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::ToolDone(_)))
                    .count(),
                1
            );
        });
    }

    #[test_case(r#"{"value":"é \\\" } ]", "nested":[{}]}"# ; "escapes_unicode_nesting")]
    #[test_case(r#"[{}, {"x": true}]"# ; "array")]
    fn completed_input_is_detected_at_every_fragment_boundary(raw: &str) {
        let expected: Value = serde_json::from_str(raw).unwrap();
        for split in raw.char_indices().map(|(index, _)| index) {
            let mut frame = InputFrame::default();
            assert!(frame.absorb(&raw[..split]).is_none());
            assert_eq!(frame.absorb(&raw[split..]), Some(expected.clone()));
            assert!(frame.absorb(raw).is_none());
        }
    }

    #[test_case("42" ; "scalar_waits")]
    #[test_case("{\"value\":\"unterminated" ; "unfinished_string")]
    #[test_case("{}{}" ; "multiple_values")]
    fn incomplete_or_ambiguous_input_never_admits(raw: &str) {
        assert!(InputFrame::default().absorb(raw).is_none());
    }

    const WRITE: &str = "file_write";
    const EDIT: &str = "file_edit";
    const PATCH: &str = "file_apply_patch";
    const BATCH: &str = "batch";
    const READ: &str = "file_read";
    const SHELL: &str = "shell";
    const MEMORY: &str = "memory";
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
            source_ordinal: None,
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
        smol::block_on(forward_provider_events(prx, Some(&sender), false, None));
        drop(sender);
        erx.drain()
            .map(|envelope| envelope.event)
            .filter(|event| matches!(event, AgentEvent::ToolInputDelta { .. }))
            .collect()
    }

    /// The early start, end to end: a batch whose whole argument lands in one
    /// fragment starts every child from that fragment, and each reports on a
    /// roster the reader was given first.
    #[test]
    fn a_batch_starts_its_children_from_the_fragment_that_closed_them() {
        let (etx, erx) = flume::unbounded();
        let sender = crate::EventSender::new(etx, 0);
        let mut ctx = crate::tools::test_support::stub_ctx_with(
            &crate::AgentMode::Build,
            Some(&sender),
            None,
        );
        ctx.registry
            .register(
                Arc::new(batch::BatchTool),
                ToolSource::Native {
                    owner: crate::tools::native::OWNER.into(),
                    contract: crate::tools::BATCH_TOOL_NAME.into(),
                    trusted: true,
                },
            )
            .unwrap();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&ran);
        ctx.local_tools = std::sync::Arc::new(std::collections::HashMap::from([(
            READ.into(),
            crate::tools::local_tool(move |_, _| {
                let count = std::sync::Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(String::new())
                })
            }),
        )]));
        let runs = Arc::new(SpeculativeRuns::new(&ctx, None));

        let (ptx, prx) = flume::unbounded();
        ptx.send(ProviderEvent::ToolUseStart {
            id: TOOL_ID.into(),
            name: BATCH.into(),
            source_ordinal: None,
        })
        .unwrap();
        ptx.send(ProviderEvent::ToolInputDelta {
            id: TOOL_ID.into(),
            delta: format!(
                r#"{{"tool_calls": [{{"tool": "{READ}", "parameters": {{"path": "a.rs"}}}}, {{"tool": "{READ}", "parameters": {{"path": "b.rs"}}}}]}}"#
            ),
        })
        .unwrap();
        drop(ptx);
        smol::block_on(async {
            forward_provider_events(prx, Some(&sender), false, Some(Arc::clone(&runs))).await;
            runs.settled().await;
        });
        drop(sender);

        let events: Vec<_> = erx.drain().map(|envelope| envelope.event).collect();
        let roster = events.iter().position(|event| {
            matches!(
                event,
                AgentEvent::ToolInputDelta {
                    roster: Some(_),
                    ..
                }
            )
        });
        let progress = events
            .iter()
            .position(|event| matches!(event, AgentEvent::BatchProgress(_)));
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both children start from the fragment that closed them"
        );
        assert!(
            roster < progress,
            "the roster is published before the children report on it: {roster:?} then {progress:?}"
        );
    }

    #[test]
    fn progress_only_forwarding_hides_model_output() {
        let (ptx, prx) = flume::unbounded();
        ptx.send(ProviderEvent::TextDelta {
            text: "private evaluator output".into(),
        })
        .unwrap();
        ptx.send(ProviderEvent::PromptProgress {
            processed: 100,
            total: 1_000,
            cache: 50,
        })
        .unwrap();
        drop(ptx);

        let (etx, erx) = flume::unbounded();
        let sender = crate::EventSender::new(etx, 0);
        let forwarded = smol::block_on(forward_provider_events(prx, Some(&sender), true, None));
        drop(sender);
        let events: Vec<_> = erx.drain().map(|envelope| envelope.event).collect();

        assert_eq!(forwarded.streamed, "private evaluator output");
        assert!(forwarded.forwarded);
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::PromptProgress {
                processed: 100,
                total: 1_000,
                cache: 50,
            }]
        ));
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
        smol::block_on(forward_provider_events(prx, Some(&sender), false, None));
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
        assert!(sizes(READ, &body(STEPPED_LINES)).is_empty());
    }

    /// A long command is as long a wait as a long write, and a closed row has
    /// only the header to say so.
    #[test]
    fn a_command_is_counted_too() {
        let mut fragments = vec![r#"{"command": "first"#];
        fragments.extend(std::iter::repeat_n(NEWLINE, STEPPED_LINES));
        assert_eq!(sizes(SHELL, &fragments), [FIRST_STEP, SECOND_STEP]);
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
        assert!(published_body(READ, &[r#"{"filePath": "a.rs"}"#]).is_empty());
    }

    /// A note is drawn while it arrives, and the row it is drawn under is the
    /// row the call settles on rather than a prefix of it.
    #[test]
    fn a_note_is_published_under_the_header_it_will_settle_on() {
        let fragments = [
            r#"{"command": "write", "path": "notes.md", "content": "one"#,
            r#"\ntwo"}"#,
        ];
        assert_eq!(published_body(MEMORY, &fragments), "one\ntwo");
        assert_eq!(
            previews(MEMORY, &fragments).last().map(String::as_str),
            Some("write notes.md")
        );
    }

    /// A preview built from several arguments need never complete, so the
    /// buffer feeding it is given up on the way one that never appears is.
    #[test]
    fn a_composite_preview_stops_buffering_past_the_scan_cap() {
        let mut pending = PendingInput::new(TOOL_ID.into(), MEMORY.into(), false);
        pending.absorb(r#"{"command": "write", "content": ""#);
        pending.absorb(&"x".repeat(tool_preview::PREVIEW_SCAN_CAP));
        assert!(pending.settled);
        assert!(pending.json.is_empty());
    }

    /// The header is one space-joined line, so the body is the only thing that
    /// can show a command the way it was written.
    #[test]
    fn a_command_is_published_as_a_body_and_previewed_as_a_line() {
        let fragments = [r#"{"command": "cd /tmp\nl"#, r#"s -la"}"#];
        assert_eq!(published_body(SHELL, &fragments), "cd /tmp\nls -la");
        assert_eq!(
            previews(SHELL, &fragments).last().map(String::as_str),
            Some("cd /tmp ls -la")
        );
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

        let forwarded = smol::block_on(forward_provider_events(rx, None, false, None));
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

        let forwarded = smol::block_on(forward_provider_events(rx, None, false, None));
        assert_eq!(forwarded.reasoning.len(), 2);
        assert_eq!(forwarded.reasoning[0].text, "first");
        assert_eq!(forwarded.reasoning[1].text, "second");
    }

    /// A prefill ping arriving mid-thought used to end the run, so one thinking
    /// block became several and kept only the first, which is why a restored
    /// transcript reported milliseconds for a thought that took seconds.
    #[test]
    fn a_prefill_ping_does_not_cut_a_thought_in_two() {
        let (tx, rx) = flume::unbounded();
        for event in [
            ProviderEvent::ThinkingDelta {
                text: "first".into(),
            },
            ProviderEvent::PromptProgress {
                processed: 1,
                total: 2,
                cache: 0,
            },
            ProviderEvent::ThinkingDelta {
                text: " second".into(),
            },
        ] {
            tx.send(event).unwrap();
        }
        drop(tx);

        let forwarded = smol::block_on(forward_provider_events(rx, None, false, None));

        assert_eq!(forwarded.reasoning.len(), 1);
        assert_eq!(forwarded.reasoning[0].text, "first second");
    }

    #[test]
    fn a_stream_without_reasoning_reports_no_durations() {
        let (tx, rx) = flume::unbounded();
        tx.send(ProviderEvent::TextDelta { text: "x".into() })
            .unwrap();
        drop(tx);

        let forwarded = smol::block_on(forward_provider_events(rx, None, false, None));
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
