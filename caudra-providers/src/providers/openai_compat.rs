use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::types::{append_tool_input, parse_tool_input};
use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::ResolvedAuth;
use crate::provider::WireRequest;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse, TokenUsage,
};

const STREAM_DONE: &str = "[DONE]";
pub(crate) const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
/// `tool_calls[].index` comes straight off the wire; a bogus huge value must
/// not size the accumulator vec.
const MAX_TOOL_CALLS_PER_MESSAGE: usize = 512;

pub(crate) struct OpenAiCompatConfig {
    pub slug: &'static str,
    pub api_key_env: &'static str,
    pub base_url: &'static str,
    pub max_tokens_field: &'static str,
    pub include_stream_usage: bool,
    pub provider_name: &'static str,
}

pub(crate) struct OpenAiCompatProvider {
    client: HttpClient,
    config: &'static OpenAiCompatConfig,
    stream_timeout: Duration,
    /// Env / `providers.toml` override, resolved once at construction. The
    /// static compat default stays the last resort because it can be more
    /// specific than the inventory one (`http://localhost:11434/v1` vs the
    /// bare ollama host). Request-time `auth.base_url` still wins (custom,
    /// local, dynamic).
    resolved_base_url: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(config: &'static OpenAiCompatConfig, timeouts: super::Timeouts) -> Self {
        let resolved_base_url = if config.slug.is_empty() {
            None
        } else {
            let providers = caudra_config::providers::ProvidersConfig::load();
            caudra_config::providers::configured_base_url(config.slug, providers.get(config.slug))
        };
        Self {
            client: super::http_client(timeouts),
            config,
            stream_timeout: timeouts.stream,
            resolved_base_url,
        }
    }

    pub(crate) fn client(&self) -> &HttpClient {
        &self.client
    }

    pub(crate) fn config(&self) -> &'static OpenAiCompatConfig {
        self.config
    }

    pub(crate) fn stream_timeout(&self) -> Duration {
        self.stream_timeout
    }

    pub(crate) async fn get_text(
        &self,
        auth: &ResolvedAuth,
        url: &str,
    ) -> Result<String, AgentError> {
        let request = auth
            .configure_request(
                Request::builder()
                    .method("GET")
                    .uri(url)
                    .header("user-agent", super::user_agent()),
            )
            .body(())?;
        let mut response = self.client.send_async(request).await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    }

    pub(crate) async fn post_text(
        &self,
        auth: &ResolvedAuth,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<String, AgentError> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(url)
            .header("user-agent", super::user_agent());
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let request = builder
            .header("content-type", content_type)
            .body(body.to_vec())?;
        let mut response = self.client.send_async(request).await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    }

    pub fn build_body(
        &self,
        model: &crate::model::Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
    ) -> Value {
        let wire_messages = convert_messages(messages, system);
        let wire_tools = convert_tools(tools);

        let mut body = json!({
            "model": model.id,
            "messages": wire_messages,
            "stream": true,
        });
        if let Some(max_output) = model.max_output_tokens {
            body[self.config.max_tokens_field] = json!(max_output);
        }
        if self.config.include_stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
            body["tools"] = wire_tools;
        }
        body
    }

    /// Effective base URL: an auth-supplied value (dynamic/custom providers)
    /// wins, then the construction-time env / `providers.toml` override, then
    /// the static compat default.
    fn base_url(&self, auth: &ResolvedAuth) -> String {
        if let Some(explicit) = auth.base_url.as_deref() {
            return explicit.to_string();
        }
        self.resolved_base_url
            .clone()
            .unwrap_or_else(|| self.config.base_url.to_string())
    }

    /// Where a Chat Completions turn posts, for the send and the dry run alike.
    pub(crate) fn chat_url(&self, auth: &ResolvedAuth) -> String {
        format!("{}{CHAT_COMPLETIONS_PATH}", self.base_url(auth))
    }

    fn build_request(
        &self,
        method: &str,
        url: &str,
        auth: &ResolvedAuth,
    ) -> isahc::http::request::Builder {
        auth.configure_request(
            Request::builder()
                .method(method)
                .uri(url)
                .header("user-agent", super::user_agent()),
        )
    }

    pub async fn do_stream(
        &self,
        model: &crate::model::Model,
        extra_headers: &[(&str, &str)],
        wire: &WireRequest,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(&wire.body)?;
        let mut request = self
            .build_request(wire.method, &wire.url, auth)
            .header("content-type", "application/json");
        for &(key, value) in extra_headers {
            request = request.header(key, value);
        }

        let request = request.body(json_body)?;

        debug!(
            model = %model.id,
            provider = self.config.provider_name,
            "sending API request"
        );

        // `connect_timeout` only covers the socket, which always succeeds
        // against a local server, and `low_speed_timeout` never starts until
        // bytes flow. Neither bounds a server that accepts the request and
        // then never answers, so the header wait gets the stream budget too.
        let response = futures_lite::future::or(
            async {
                self.client
                    .send_async(request)
                    .await
                    .map_err(AgentError::from)
            },
            async {
                smol::Timer::after(self.stream_timeout).await;
                Err(AgentError::Timeout {
                    secs: self.stream_timeout.as_secs(),
                })
            },
        )
        .await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(
                BufReader::new(response.into_body()),
                event_tx,
                self.stream_timeout,
            )
            .await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    pub async fn fetch_and_parse_models(
        &self,
        auth: &ResolvedAuth,
        parse_fn: impl Fn(&Value) -> Option<crate::model::ModelInfo>,
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let base = self.base_url(auth);
        let url = format!("{base}/models");
        let body_text = self.get_text(auth, &url).await?;
        let body: Value = serde_json::from_str(&body_text)?;

        let mut models: Vec<crate::model::ModelInfo> = body["data"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_fn).collect())
            .unwrap_or_default();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    fn default_model_parser(m: &Value) -> Option<crate::model::ModelInfo> {
        let id = m["id"].as_str()?;
        let context_window = m["context_length"]
            .as_u64()
            .or_else(|| m["max_model_len"].as_u64())
            .or_else(|| m["max_context_length"].as_u64())
            .and_then(|v| u32::try_from(v).ok());
        let max_output_tokens = m["max_tokens"]
            .as_u64()
            .or_else(|| m["max_output_length"].as_u64())
            .and_then(|v| u32::try_from(v).ok());
        let supports_vision = m["input_modalities"]
            .as_array()
            .map(|mods| mods.iter().any(|v| v.as_str() == Some("image")));
        let pricing = m["pricing"].as_object().and_then(|p| {
            Some(crate::model::ModelPricing {
                input: p.get("prompt")?.as_str()?.parse().ok()?,
                output: p.get("completion")?.as_str()?.parse().ok()?,
                cache_write: p
                    .get("cache_creation")?
                    .as_str()?
                    .parse::<f64>()
                    .ok()
                    .unwrap_or(0.0),
                cache_read: p
                    .get("cache_read")?
                    .as_str()?
                    .parse::<f64>()
                    .ok()
                    .unwrap_or(0.0),
                fast: None,
                tiers: crate::model::ModelPricing::UNTIERED,
            })
        });
        Some(crate::model::ModelInfo {
            id: id.to_string(),
            context_window,
            max_output_tokens,
            pricing,
            supports_thinking: None,
            supports_vision,
            reasoning_options: None,
            provider_info: None,
        })
    }

    pub async fn do_list_models(
        &self,
        auth: &ResolvedAuth,
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        self.fetch_and_parse_models(auth, Self::default_model_parser)
            .await
    }
}

pub fn convert_messages(messages: &[Message], system: &str) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];

    for msg in messages {
        match msg.role {
            Role::User => {
                let mut tool_results = Vec::new();
                let mut text_parts: Vec<&str> = Vec::new();
                let mut image_parts = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::Image { source } => {
                            image_parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": source.to_data_url() }
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                // Tool messages must directly follow the assistant's
                // tool_calls, before any user content.
                out.extend(tool_results);
                if !image_parts.is_empty() {
                    let mut parts = image_parts;
                    if !text_parts.is_empty() {
                        parts.push(json!({"type": "text", "text": text_parts.join("\n")}));
                    }
                    out.push(json!({"role": "user", "content": parts}));
                } else if !text_parts.is_empty() {
                    out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut reasoning_text = String::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::Thinking { thinking, .. } => {
                            reasoning_text.push_str(thinking);
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                if !text.is_empty() || !tool_calls.is_empty() || !reasoning_text.is_empty() {
                    // Always emit string `content` (""): some OpenAI-compatible
                    // backends (e.g. Cloudflare Workers AI gpt-oss) reject
                    // omitted/null content on assistant tool-call messages.
                    let mut msg_obj = json!({"role": "assistant", "content": text});
                    if !reasoning_text.is_empty() {
                        msg_obj["reasoning_content"] = Value::String(reasoning_text);
                    }
                    if !tool_calls.is_empty() {
                        msg_obj["tool_calls"] = Value::Array(tool_calls);
                    }
                    out.push(msg_obj);
                }
            }
        }
    }

    out
}

pub fn convert_tools(anthropic_tools: &Value) -> Value {
    let Some(tools) = anthropic_tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name")?,
                        "description": t.get("description")?,
                        "parameters": t.get("input_schema")?,
                    }
                }))
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<ContentDelta>,
    reasoning_content: Option<String>,
    /// vLLM sends `reasoning` instead of `reasoning_content`, and AxonHub
    /// sends both, so a serde alias would fail on the duplicate field.
    reasoning: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum ContentDelta {
    Array(Vec<ContentDeltaPart>),
    String(String),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ContentDeltaPart {
    Text { text: String },
    Thinking { thinking: Vec<ThinkingDelta> },
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum ThinkingDelta {
    Block(ThinkingDeltaBlock),
    String(String),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ThinkingDeltaBlock {
    Text { text: String },
}

#[derive(Deserialize)]
struct ChunkChoice {
    delta: Option<ChunkDelta>,
    #[serde(default)]
    message: Option<ChunkDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek reports cache hits here instead of `prompt_tokens_details`.
    #[serde(default)]
    prompt_cache_hit_tokens: u32,
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<ChunkUsage>,
}

struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
    sent_bytes: usize,
}

async fn publish_completed_inputs(
    calls: &[ToolAccumulator],
    event_tx: &Sender<ProviderEvent>,
    complete: bool,
) -> Result<(), AgentError> {
    for call in calls {
        if call.id.is_empty() || call.name.is_empty() {
            continue;
        }
        let (input, invalid_input) = parse_tool_input(&call.arguments, complete);
        event_tx
            .send_async(ProviderEvent::ToolInputReady {
                id: call.id.clone(),
                name: call.name.clone(),
                input,
                invalid_input,
            })
            .await?;
    }
    Ok(())
}

pub async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();

    let mut text = String::new();
    let mut reasoning_text = String::new();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut is_first_content = true;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };

        if data == STREAM_DONE {
            break;
        }

        if data.contains("\"error\"")
            && let Ok(ev) = serde_json::from_str::<super::SseErrorPayload>(data)
        {
            warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error in stream");
            return Err(ev.into_agent_error());
        }

        let chunk: SseChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, raw_sse = %data, "failed to parse SSE chunk");
                continue;
            }
        };

        if let Some(u) = chunk.usage {
            let cached = u
                .prompt_tokens_details
                .map_or(0, |d| d.cached_tokens)
                .max(u.prompt_cache_hit_tokens);
            usage = TokenUsage {
                input: u.prompt_tokens.saturating_sub(cached),
                output: u.completion_tokens,
                cache_read: cached,
                cache_creation: 0,
            };
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        if let Some(reason) = choice.finish_reason {
            stop_reason = Some(StopReason::from_openai(&reason));
        }

        let Some(delta) = choice.delta.or(choice.message) else {
            if let Some(reason) = stop_reason {
                publish_completed_inputs(
                    &tool_accumulators,
                    event_tx,
                    reason != StopReason::MaxTokens,
                )
                .await?;
            }
            continue;
        };

        if let Some(reasoning) = [delta.reasoning_content, delta.reasoning]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
        {
            reasoning_text.push_str(&reasoning);
            event_tx
                .send_async(ProviderEvent::ThinkingDelta { text: reasoning })
                .await?;
        }

        match delta.content {
            Some(ContentDelta::String(content_str)) if !content_str.is_empty() => {
                let content = if is_first_content {
                    is_first_content = false;
                    content_str.trim_start().to_string()
                } else {
                    content_str
                };

                if !content.is_empty() {
                    text.push_str(&content);
                    event_tx
                        .send_async(ProviderEvent::TextDelta { text: content })
                        .await?;
                }
            }
            Some(ContentDelta::Array(content_array)) => {
                for part in content_array {
                    match part {
                        ContentDeltaPart::Thinking { thinking } => {
                            for thinking_block in thinking {
                                let content = match thinking_block {
                                    ThinkingDelta::Block(ThinkingDeltaBlock::Text {
                                        text: content_str,
                                    }) => content_str,
                                    ThinkingDelta::String(content_str) => content_str,
                                };

                                if content.is_empty() {
                                    continue;
                                }

                                reasoning_text.push_str(&content);
                                event_tx
                                    .send_async(ProviderEvent::ThinkingDelta { text: content })
                                    .await?;
                            }
                        }
                        ContentDeltaPart::Text { text: content_str } => {
                            let content = if is_first_content {
                                is_first_content = false;
                                content_str.trim_start().to_string()
                            } else {
                                content_str
                            };

                            if !content.is_empty() {
                                text.push_str(&content);
                                event_tx
                                    .send_async(ProviderEvent::TextDelta { text: content })
                                    .await?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if let Some(tc_deltas) = delta.tool_calls {
            for tc in tc_deltas {
                if tc.index >= MAX_TOOL_CALLS_PER_MESSAGE {
                    warn!(index = tc.index, "ignoring out-of-range tool call index");
                    continue;
                }
                while tool_accumulators.len() <= tc.index {
                    tool_accumulators.push(ToolAccumulator {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        announced: false,
                        sent_bytes: 0,
                    });
                }
                let acc = &mut tool_accumulators[tc.index];
                if let Some(id) = tc.id
                    && !acc.announced
                {
                    acc.id = id;
                }
                // GLM-5.2 via Mistral sends "" names in subsequent chunks; skip to keep the accumulated name.
                if let Some(func) = tc.function {
                    if let Some(name) = func.name
                        && !name.is_empty()
                    {
                        acc.name = name;
                    }
                    if let Some(args) = func.arguments
                        && !args.is_empty()
                    {
                        append_tool_input(&mut acc.arguments, &args);
                    }
                }
            }
            for (ordinal, acc) in tool_accumulators.iter_mut().enumerate() {
                if acc.name.is_empty() || acc.id.is_empty() {
                    continue;
                }
                if !acc.announced {
                    acc.announced = true;
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: acc.id.clone(),
                            name: acc.name.clone(),
                            source_ordinal: Some(ordinal),
                        })
                        .await?;
                }
                // After the start, since one chunk can carry both and the
                // consumer keys deltas off the id the start announced.
                if acc.sent_bytes < acc.arguments.len() {
                    let delta = acc.arguments[acc.sent_bytes..].to_owned();
                    acc.sent_bytes = acc.arguments.len();
                    event_tx
                        .send_async(ProviderEvent::ToolInputDelta {
                            id: acc.id.clone(),
                            delta,
                        })
                        .await?;
                }
            }
        }
        if let Some(reason) = stop_reason {
            publish_completed_inputs(
                &tool_accumulators,
                event_tx,
                reason != StopReason::MaxTokens,
            )
            .await?;
        }
    }

    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_text.is_empty() {
        content_blocks.push(ContentBlock::thinking(reasoning_text, None));
    }

    if !text.is_empty() {
        content_blocks.push(ContentBlock::Text { text });
    }

    let mut invalid_tool_inputs = HashMap::new();
    for (idx, acc) in tool_accumulators.into_iter().enumerate() {
        let (input, invalid_input) = parse_tool_input(
            &acc.arguments,
            stop_reason.is_some_and(|reason| reason != StopReason::MaxTokens),
        );
        let id = if acc.id.is_empty() {
            warn!(
                input_bytes = acc.arguments.len(),
                "provider sent empty tool_use id; substituting placeholder"
            );
            format!("caudra_unnamed_{idx}")
        } else {
            acc.id
        };
        let name = if acc.name.is_empty() {
            warn!(%id, input_bytes = acc.arguments.len(), "provider sent empty tool_use name; substituting placeholder");
            "caudra_unknown_tool".to_owned()
        } else {
            acc.name
        };
        if !acc.announced {
            event_tx
                .send_async(ProviderEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                    source_ordinal: Some(idx),
                })
                .await?;
        }
        event_tx
            .send_async(ProviderEvent::ToolInputReady {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                invalid_input: invalid_input.clone(),
            })
            .await?;
        if let Some(invalid) = invalid_input {
            invalid_tool_inputs.insert(id.clone(), invalid);
        }
        content_blocks.push(ContentBlock::tool_use(id, name, input));
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
        invalid_tool_inputs,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invalid_tool_input;
    use crate::providers::test_support::{
        PEER_ATTACK, PEER_TEXT, assert_peer_framing, automation_event_origin, peer_message_origin,
        task_event_origin, task_observation_with_output_refs, workflow_event_origin,
    };
    use crate::{StandingReminderKind, SteeringKind};
    use futures_lite::io::Cursor;
    use test_case::test_case;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    const STEERING_TEXT: &str = "Continue with a useful response.";
    const STEERING_RULE: &str = "empty_output";
    const INVALID_CALL_ID: &str = "original-invalid-call";
    const VALID_CALL_ID: &str = "original-valid-call";
    const TOOL_NAME: &str = "read";
    const PRIVATE_TAIL: &str = "private repair source tail";
    const MALFORMED_COMMAND: &str = r#"{"command":"echo safe""#;
    const STREAM_TAIL: &str = "tail after independently executable call";
    const EARLY_ARGUMENTS: &str = r#"{"path":"early"}"#;
    const EARLY_ARGUMENT_PREFIX: &str = r#"{"path":"#;
    const LATE_ARGUMENTS: &str = r#"{"path":"late"}"#;

    #[test_case(Some("tool_calls"), true ; "completed_response")]
    #[test_case(Some("length"), false ; "token_truncated")]
    #[test_case(None, false ; "transport_truncated")]
    fn malformed_arguments_require_final_status(reason: Option<&str>, complete: bool) {
        smol::block_on(async {
            let call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":INVALID_CALL_ID,"function":{"name":TOOL_NAME,"arguments":MALFORMED_COMMAND}}]}}]});
            let mut sse = format!("data: {call}\n\n");
            if let Some(reason) = reason {
                let finish = json!({"choices":[{"finish_reason":reason}]});
                sse.push_str(&format!("data: {finish}\n\n"));
            }
            let (tx, _rx) = flume::unbounded();
            let response = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            let invalid = &response.invalid_tool_inputs[INVALID_CALL_ID];
            assert_eq!(invalid.raw, MALFORMED_COMMAND);
            assert_eq!(invalid.complete, complete);
            assert!(!invalid.clipped);
        });
    }

    #[test_case(r#"{"INVALID_JSON":"display","caudra_invalid_json_raw":"{\"command\":\"embedded\"}","caudra_invalid_json_complete":true,"caudra_invalid_json_clipped":false,"command":"actual"}"# ; "spoofed_metadata")]
    fn valid_marker_fields_survive_wire_projection(raw: &str) {
        let input: Value = serde_json::from_str(raw).unwrap();
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                VALID_CALL_ID,
                TOOL_NAME,
                input.clone(),
            )],
            ..Message::default()
        };
        let wire = convert_messages(&[message], "");
        assert_eq!(
            serde_json::from_str::<Value>(
                wire[1]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            input
        );
    }

    #[test_case(true, false ; "missing_name")]
    #[test_case(false, true ; "missing_id")]
    #[test_case(false, false ; "missing_both")]
    fn late_identity_does_not_block_independent_starts(has_id: bool, has_name: bool) {
        smol::block_on(async {
            let mut early = json!({"index":0,"function":{"arguments": EARLY_ARGUMENT_PREFIX}});
            if has_id {
                early["id"] = json!(INVALID_CALL_ID);
            }
            if has_name {
                early["function"]["name"] = json!(TOOL_NAME);
            }
            let chunks = [
                json!({"choices":[{"delta":{"tool_calls":[early,{"index":1,"id":VALID_CALL_ID,"function":{"name":TOOL_NAME,"arguments":LATE_ARGUMENTS}}]}}]}),
                json!({"choices":[{"delta":{"content":STREAM_TAIL}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":INVALID_CALL_ID,"function":{"name":TOOL_NAME,"arguments": &EARLY_ARGUMENTS[EARLY_ARGUMENT_PREFIX.len()..]}}]}}]}),
                json!({"choices":[{"finish_reason":"tool_calls"}]}),
            ];
            let sse = chunks
                .iter()
                .map(|chunk| format!("data: {chunk}\n\n"))
                .collect::<String>();
            let (tx, rx) = flume::unbounded();
            let response = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            let events: Vec<_> = rx.drain().collect();
            assert!(
                matches!(&events[0], ProviderEvent::ToolUseStart { id, source_ordinal: Some(1), .. } if id == VALID_CALL_ID)
            );
            assert!(
                matches!(&events[1], ProviderEvent::ToolInputDelta { id, delta } if id == VALID_CALL_ID && delta == LATE_ARGUMENTS)
            );
            assert!(matches!(&events[2], ProviderEvent::TextDelta { text } if text == STREAM_TAIL));
            let starts: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    ProviderEvent::ToolUseStart {
                        id, source_ordinal, ..
                    } => Some((id.as_str(), *source_ordinal)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                starts,
                [(VALID_CALL_ID, Some(1)), (INVALID_CALL_ID, Some(0))]
            );
            let deltas: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    ProviderEvent::ToolInputDelta { id, delta } => {
                        Some((id.as_str(), delta.as_str()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                deltas,
                [
                    (VALID_CALL_ID, LATE_ARGUMENTS),
                    (INVALID_CALL_ID, EARLY_ARGUMENTS)
                ]
            );
            assert_eq!(
                response.message.tool_uses().collect::<Vec<_>>(),
                [
                    (
                        INVALID_CALL_ID,
                        TOOL_NAME,
                        &serde_json::from_str::<Value>(EARLY_ARGUMENTS).unwrap()
                    ),
                    (
                        VALID_CALL_ID,
                        TOOL_NAME,
                        &serde_json::from_str::<Value>(LATE_ARGUMENTS).unwrap()
                    ),
                ]
            );
        });
    }

    #[test]
    fn malformed_repair_source_is_not_reprojected_to_the_model() {
        let raw = format!(
            "{}{}",
            "x".repeat(crate::MAX_TOOL_INPUT_BYTES / 2),
            PRIVATE_TAIL
        );
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                INVALID_CALL_ID,
                TOOL_NAME,
                invalid_tool_input(&raw),
            )],
            ..Message::default()
        };
        let wire = convert_messages(std::slice::from_ref(&message), "");
        assert!(!serde_json::to_string(&wire).unwrap().contains(PRIVATE_TAIL));
        assert_eq!(
            message
                .tool_uses()
                .next()
                .unwrap()
                .2
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }

    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Recovery) ; "recovery")]
    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Advisory) ; "advisory")]
    #[test_case(Message::task_observation(STEERING_TEXT.into(), task_event_origin()) ; "task_event")]
    #[test_case(task_observation_with_output_refs(STEERING_TEXT); "retained_task_outputs")]
    #[test_case(Message::workflow_observation(STEERING_TEXT.into(), workflow_event_origin()) ; "workflow_event")]
    #[test_case(Message::automation_observation(STEERING_TEXT.into(), automation_event_origin()) ; "automation_event")]
    #[test_case(Message::standing_reminder(STEERING_TEXT.into(), StandingReminderKind::BackgroundWork) ; "background_reminder")]
    fn observation_metadata_is_not_on_wire(message: Message) {
        let wire = convert_messages(&[message], "");
        assert_eq!(
            wire,
            vec![
                json!({"role": "system", "content": ""}),
                json!({"role": "user", "content": STEERING_TEXT})
            ]
        );
    }

    #[test_case(PEER_TEXT ; "plain_text")]
    #[test_case(PEER_ATTACK ; "adversarial_host_markers")]
    fn peer_observation_wire_contains_only_framed_text(text: &str) {
        let origin = peer_message_origin();
        let wire = convert_messages(
            &[Message::peer_observation(text.into(), origin.clone())],
            "",
        );
        let framed = wire[1]["content"].as_str().unwrap();
        assert_peer_framing(framed, text, &origin);
        assert_eq!(
            wire,
            vec![
                json!({"role": "system", "content": ""}),
                json!({"role": "user", "content": framed})
            ]
        );
    }

    #[test_case("{broken" ; "malformed")]
    #[test_case("{\"path\":" ; "truncated")]
    #[test_case("" ; "empty")]
    #[test_case("  " ; "whitespace")]
    fn invalid_tool_arguments_preserve_siblings_and_pairing(raw: &str) {
        smol::block_on(async {
            let chunk = json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": INVALID_CALL_ID, "function": {"name": TOOL_NAME, "arguments": raw}},
                {"index": 1, "id": VALID_CALL_ID, "function": {"name": TOOL_NAME, "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"}]});
            let sse = format!("data: {chunk}\n\ndata: {STREAM_DONE}\n");
            let (tx, _rx) = flume::unbounded();
            let response = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            let tools: Vec<_> = response.message.tool_uses().collect();
            assert_eq!(
                tools,
                vec![
                    (INVALID_CALL_ID, TOOL_NAME, &invalid_tool_input(raw)),
                    (VALID_CALL_ID, TOOL_NAME, &json!({})),
                ]
            );
            assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
            let result = Message {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: INVALID_CALL_ID.into(),
                    content: String::new(),
                    is_error: true,
                    output_ref: None,
                }],
                ..Default::default()
            };
            let wire = convert_messages(&[response.message, result], "");
            assert_eq!(wire[1]["tool_calls"][0]["id"], INVALID_CALL_ID);
            assert_eq!(wire[1]["tool_calls"][1]["id"], VALID_CALL_ID);
            assert_eq!(wire[2]["tool_call_id"], INVALID_CALL_ID);
        });
    }

    #[test]
    fn default_model_parser_reads_context_and_output_length() {
        let m = json!({"id": "m", "context_length": 524_288, "max_output_length": 65_536});
        let info = OpenAiCompatProvider::default_model_parser(&m).unwrap();
        assert_eq!(info.context_window, Some(524_288));
        assert_eq!(info.max_output_tokens, Some(65_536));
    }

    #[test]
    fn default_model_parser_missing_pricing_stays_none() {
        let info = OpenAiCompatProvider::default_model_parser(&json!({"id": "m"})).unwrap();
        assert!(info.pricing.is_none());
    }

    #[test_case(json!({"id": "m", "input_modalities": ["text", "image"]}), Some(true) ; "image_modality_enables_vision")]
    #[test_case(json!({"id": "m", "input_modalities": ["text"]}), Some(false) ; "text_only_disables_vision")]
    #[test_case(json!({"id": "m"}), None ; "missing_modalities_stays_unknown")]
    fn default_model_parser_vision_flag(m: Value, expected: Option<bool>) {
        let info = OpenAiCompatProvider::default_model_parser(&m).unwrap();
        assert_eq!(info.supports_vision, expected);
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":40}}}\n\
\n\
data: [DONE]\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(resp.usage.input, 60);
            assert_eq!(resp.usage.output, 10);
            assert_eq!(resp.usage.cache_read, 40);
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert!(!resp.message.has_tool_calls());

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text } = e {
                    deltas.push(text);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_deepseek_cache_hit_tokens() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_cache_hit_tokens\":80,\"prompt_cache_miss_tokens\":20}}\n\
\n\
data: [DONE]\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(resp.usage.input, 20);
            assert_eq!(resp.usage.cache_read, 80);
            assert_eq!(resp.usage.output, 10);
        })
    }

    #[test]
    fn parse_sse_reasoning_and_content() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"...\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\
\n\
data: [DONE]\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think...")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let mut thinking = Vec::new();
            let mut text_deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                match e {
                    ProviderEvent::ThinkingDelta { text } => thinking.push(text),
                    ProviderEvent::TextDelta { text } => text_deltas.push(text),
                    ProviderEvent::ToolUseStart { .. } => {}
                    ProviderEvent::ToolInputDelta { .. } => {}
                    ProviderEvent::PromptProgress { .. } => {}
                    ProviderEvent::ThinkingBoundary => {}
                    ProviderEvent::ToolAliases { .. } | ProviderEvent::ToolInputReady { .. } => {}
                }
            }
            assert_eq!(thinking, vec!["Let me think", "..."]);
            assert_eq!(text_deltas, vec!["Hello"]);
        })
    }

    #[test_case(r#"{"reasoning_content":"think","reasoning":"ignored"}"#; "prefers_reasoning_content")]
    #[test_case(r#"{"reasoning":"think"}"#; "reasoning_only")]
    #[test_case(r#"{"reasoning_content":"","reasoning":"think"}"#; "empty_reasoning_content_falls_back")]
    fn parse_sse_proxy_reasoning_variants(delta: &str) {
        smol::block_on(async {
            let sse = format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\ndata: [DONE]\n");

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "think")
            );
        })
    }

    #[test]
    fn convert_messages_structure() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "thinking...".to_string(),
                    },
                    ContentBlock::tool_use("tc_1", "bash", json!({"command": "ls"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "file.txt".to_string(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages, "be helpful");

        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "be helpful");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"], "hello");
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["content"], "thinking...");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "tc_1");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "tc_1");
        assert_eq!(wire[3]["content"], "file.txt");
    }

    #[test]
    fn convert_messages_assistant_tool_calls_only_has_content() {
        let messages = vec![
            Message::user("list files".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "tc_1",
                    "bash",
                    json!({"command": "ls"}),
                )],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages, "be helpful");

        assert_eq!(wire[2]["role"], "assistant");
        // `content` must be a present string ("") even with only tool_calls;
        // strict OpenAI-compatible backends reject null/omitted content.
        assert_eq!(wire[2]["content"], "");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn convert_tools_structure() {
        let anthropic = json!([{
            "name": "bash",
            "description": "Run a command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }]);

        let openai = convert_tools(&anthropic);
        let tool = &openai[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "bash");
        assert_eq!(tool["function"]["description"], "Run a command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_sse_multiple_parallel_tool_calls() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\
\n\
data: [DONE]\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 2);
            assert_eq!(tools[0].0, "c1");
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!(tools[1].0, "c2");
            assert_eq!(tools[1].1, "read");
            assert_eq!(tools[1].2["path"], "/tmp");
            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name, .. } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                starts,
                vec![("c1".into(), "bash".into()), ("c2".into(), "read".into()),]
            );
        })
    }

    #[test]
    fn parse_sse_error_payload_returns_err() {
        smol::block_on(async {
            let sse = "\
data: {\"error\":{\"message\":\"Server overloaded\",\"type\":\"overloaded_error\"}}\n";

            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();

            match err {
                AgentError::Api {
                    status, message, ..
                } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Server overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_empty_tool_id_and_name_get_placeholders() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"tool_calls\\\":[{\\\"tool\\\":\\\"read\\\"}]}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
\n\
data: [DONE]\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert!(!tools[0].0.is_empty(), "id must be non-empty for Bedrock");
            assert!(!tools[0].1.is_empty(), "name must be non-empty for Bedrock");
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_preserves_invalid_input() {
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{broken\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
\n\
data: [DONE]\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].0, "c1");
            assert_eq!(*tools[0].2, invalid_tool_input("{broken"));
        })
    }

    #[test]
    fn parse_sse_empty_name_in_subsequent_chunks_preserves_first_name() {
        // GLM-5.2 via Mistral sends the tool name in the first chunk and "" in
        // subsequent chunks. The accumulated name must not be overwritten.
        smol::block_on(async {
            let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tc_1\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"\",\"arguments\":\"{\\\"path\\\": \\\"/tmp\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"\",\"arguments\":\"/file\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
\n\
data: [DONE]\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(tools[0].2["path"], "/tmp/file");
        })
    }

    #[test]
    fn convert_messages_user_with_image() {
        use crate::types::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msgs = vec![Message::user_with_images("describe".into(), vec![source])];
        let result = convert_messages(&msgs, "system");
        let user = &result[1];
        let content = user["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image_url");
        assert!(
            content[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "describe");
    }

    #[test]
    fn convert_messages_tool_results_precede_tool_returned_image() {
        use crate::types::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Image {
                    source: ImageSource::new(ImageMediaType::Png, Arc::from("abc123")),
                },
            ],
            ..Default::default()
        }];
        let result = convert_messages(&msgs, "system");
        assert_eq!(result[1]["role"], "tool");
        assert_eq!(result[1]["tool_call_id"], "t1");
        assert_eq!(result[2]["role"], "user");
        assert_eq!(result[2]["content"][0]["type"], "image_url");
    }

    #[test]
    fn convert_messages_user_text_only_stays_string() {
        let msgs = vec![Message::user("hello".into())];
        let result = convert_messages(&msgs, "system");
        assert!(result[1]["content"].is_string());
    }

    #[test]
    fn convert_messages_assistant_with_reasoning() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("Let me think...".into(), None),
                ContentBlock::Text {
                    text: "Hello".into(),
                },
            ],
            ..Default::default()
        }];
        let wire = convert_messages(&messages, "");
        let asst = &wire[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["content"], "Hello");
        assert_eq!(asst["reasoning_content"], "Let me think...");
    }

    #[test]
    fn convert_messages_assistant_reasoning_only() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::thinking("Just thinking...".into(), None)],
            ..Default::default()
        }];
        let wire = convert_messages(&messages, "");
        let asst = &wire[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["reasoning_content"], "Just thinking...");
        assert_eq!(asst["content"], "");
    }

    #[test]
    fn parse_sse_empty_stream() {
        smol::block_on(async {
            let sse = "data: [DONE]\n";
            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            assert!(resp.message.content.is_empty());
            assert_eq!(resp.usage, TokenUsage::default());
            assert_eq!(resp.stop_reason, None);
        })
    }

    #[test]
    fn parse_sse_content_as_array_with_thinking() {
        smol::block_on(async {
            // Test parsing content as an array with thinking blocks
            let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"Let me think\"}]}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"...\"}]}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: [DONE]\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think..."),
                "{:?}",
                resp.message.content[0],
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let mut thinking_deltas = Vec::new();
            let mut text_deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                match e {
                    ProviderEvent::ThinkingDelta { text } => thinking_deltas.push(text),
                    ProviderEvent::TextDelta { text } => text_deltas.push(text),
                    _ => {}
                }
            }

            assert_eq!(text_deltas, vec!["Hello"]);
            assert_eq!(thinking_deltas, vec!["Let me think", "..."]);
        })
    }
}
