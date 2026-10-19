mod json_schema;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudra_storage::id::CaudraId;
use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use jiff::civil;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::accounts::AccountCredential;
use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, StaticReasoningOption};
use crate::pricing::PriceChange;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, ThinkingConfig, TokenUsage,
};

use super::{ResolvedAuth, auth_proxy_route, http_client, next_sse_line, proxy_route};

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const ENV_VAR: &str = "GEMINI_API_KEY";
const ONE_MILLION_WINDOW: u32 = 1_048_576;
/// The output limit of every Gemini model in the table. On Gemini 4 it is the
/// most one streaming request returns. Asking for more starts long decode,
/// which stops with `CONTINUATION` and expects the client to resume.
const MAX_OUTPUT_TOKENS: u32 = 65_536;
const ARGON: &str = "gemini-4-argon";
const FLASH_3_8: &str = "gemini-3.8-flash";
const FLASH_3_6: &str = "gemini-3.6-flash";
/// Gemini 4 and 3 take a `thinkingLevel` and cannot turn thinking off, so
/// `off` runs at the lowest level. Argon and 3.8 Flash reject `minimal`.
const LEVELS_LOW_TO_HIGH: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "medium", "high"])];
const LEVELS_MINIMAL_TO_HIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "minimal", "low", "medium", "high",
])];
/// What Google's FAQ says to send in place of a thought signature on a
/// function call the model did not produce, such as one carried over from
/// another model's turn. Gemini 3 and later reject such a call otherwise.
const SKIP_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
/// The first Gemini major version that validates thought signatures.
const VALIDATES_SIGNATURES_SINCE: u32 = 3;
const GEMINI_ID_PREFIX: &str = "gemini-";
/// Ids without a major version that still name a Gemini 2.5 model.
const UNVERSIONED_GEMINI_2: [&str; 1] = ["robotics-er-1.5"];
const TOO_MANY_REQUESTS: u16 = 429;
/// Google refuses a model the key's tier does not serve, such as Argon on the
/// free tier, with a 429 that lists every quota it hit as `limit: 0`.
const QUOTA_LIMIT_LABEL: &str = "limit: ";
const ZERO_QUOTA_LIMIT: &str = "0";
/// Reported as a billing refusal, which neither the retry loop nor a goal
/// retries: no wait raises a limit of 0.
const PAYMENT_REQUIRED: u16 = 402;
const FLASH_PROMO_PRICE: ModelPricing = ModelPricing {
    input: 0.75,
    output: 3.75,
    cache_write: 0.00,
    cache_read: 0.075,
    fast: None,
    tiers: ModelPricing::UNTIERED,
};
const FLASH_LIST_PRICE: ModelPricing = ModelPricing {
    input: 1.50,
    output: 7.50,
    cache_write: 0.00,
    cache_read: 0.15,
    fast: None,
    tiers: ModelPricing::UNTIERED,
};

/// Introductory rates Google ends on a set day, from
/// <https://ai.google.dev/gemini-api/docs/pricing>.
pub(crate) const PRICE_CHANGES: &[PriceChange] = &[
    PriceChange {
        prefix: ARGON,
        from: civil::date(2026, 12, 1),
        pricing: ModelPricing {
            input: 4.00,
            output: 20.00,
            cache_write: 0.00,
            cache_read: 0.20,
            fast: None,
            tiers: ModelPricing::UNTIERED,
        },
    },
    PriceChange {
        prefix: FLASH_3_8,
        from: civil::date(2027, 1, 1),
        pricing: FLASH_LIST_PRICE,
    },
    PriceChange {
        prefix: FLASH_3_6,
        from: civil::date(2027, 1, 1),
        pricing: FLASH_LIST_PRICE,
    },
];

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "google",
    display_name: "Google",
    protocol: caudra_config::providers::Protocol::Google,
    default_base_url: BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "google/gemini-4-argon",
    plans: None,
    login_url: Some("https://aistudio.google.com/apikey"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &[ARGON],
            small: false,
            family: ModelFamily::Gemini,
            vision: true,
            default: true,
            // Introductory through 2026-11-30, see `PRICE_CHANGES`.
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 0.00,
                cache_read: 0.10,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            context_window: ONE_MILLION_WINDOW,
            reasoning_options: Some(LEVELS_LOW_TO_HIGH),
        },
        ModelEntry {
            prefixes: &[FLASH_3_8],
            small: false,
            family: ModelFamily::Gemini,
            vision: true,
            default: false,
            // Introductory through 2026-12-31, see `PRICE_CHANGES`.
            pricing: FLASH_PROMO_PRICE,
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            context_window: ONE_MILLION_WINDOW,
            reasoning_options: Some(LEVELS_LOW_TO_HIGH),
        },
        ModelEntry {
            prefixes: &[FLASH_3_6],
            small: false,
            family: ModelFamily::Gemini,
            vision: true,
            default: false,
            // Introductory through 2026-12-31, see `PRICE_CHANGES`.
            pricing: FLASH_PROMO_PRICE,
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            context_window: ONE_MILLION_WINDOW,
            reasoning_options: Some(LEVELS_MINIMAL_TO_HIGH),
        },
        ModelEntry {
            prefixes: &["gemini-3.5-flash-lite"],
            small: true,
            family: ModelFamily::Gemini,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.30,
                output: 2.50,
                cache_write: 0.00,
                cache_read: 0.03,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            context_window: ONE_MILLION_WINDOW,
            reasoning_options: Some(LEVELS_MINIMAL_TO_HIGH),
        },
    ];
    MODELS
}

fn resolve_google_base_url() -> Option<String> {
    let config = caudra_config::providers::ProvidersConfig::load();
    caudra_config::providers::resolve_base_url("google", config.get("google"))
}

fn resolve_auth_from_key(key: &str, base_url: Option<String>) -> ResolvedAuth {
    ResolvedAuth {
        base_url,
        headers: vec![("x-goog-api-key".into(), key.to_string())],
    }
}

pub struct Google {
    client: HttpClient,
    auth: Arc<Mutex<ResolvedAuth>>,
    stream_timeout: Duration,
}

impl Google {
    pub fn new(account: &AccountCredential, timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let resolved = resolve_auth_from_key(account.api_key()?, resolve_google_base_url());
        Ok(Self {
            client: http_client(timeouts, &proxy_route(resolved.base_url.as_deref())),
            auth: Arc::new(Mutex::new(resolved)),
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: http_client(timeouts, &auth_proxy_route(&auth)),
            auth,
            stream_timeout: timeouts.stream,
        }
    }

    fn build_request(&self, method: &str, url: &str) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        auth.configure_request(
            Request::builder()
                .method(method)
                .uri(url)
                .header("user-agent", super::user_agent()),
        )
    }

    fn api_key(&self) -> String {
        let auth = self.auth.lock().unwrap();
        auth.headers
            .iter()
            .find(|(k, _)| k == "x-goog-api-key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    fn models_url(&self) -> String {
        let base = {
            let auth = self.auth.lock().unwrap();
            auth.base_url.as_deref().unwrap_or(BASE_URL).to_string()
        };
        let key = self.api_key();
        format!("{base}/models?key={key}&pageSize=1000")
    }

    /// What one turn posts with `auth`, for the send and the dry run alike.
    /// The key rides in `x-goog-api-key`, so the URL never carries it.
    fn request(
        &self,
        auth: &ResolvedAuth,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        thinking: &ThinkingConfig,
    ) -> WireRequest {
        let base = auth.base_url.as_deref().unwrap_or(BASE_URL);
        let encoded = super::urlenc(&model.id);
        WireRequest::post(
            format!("{base}/models/{encoded}:streamGenerateContent?alt=sse"),
            self.build_body(model, messages, system, tools, thinking),
        )
    }

    fn build_body(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        thinking: &ThinkingConfig,
    ) -> Value {
        let mut contents = convert_messages(messages);
        if validates_thought_signatures(&model.id) {
            sign_unsigned_calls(&mut contents);
        }
        let mut body = json!({
            "contents": contents,
        });

        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }

        thinking.apply_google_thinking(&mut body, model);

        if let Some(max_output) = model.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output);
        }

        let tool_decls = convert_tools(tools);
        if !tool_defs_empty(&tool_decls) {
            body["tools"] = json!([{"functionDeclarations": tool_decls}]);
        }

        body
    }

    async fn do_stream(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let wire = self.request(
            &self.current_auth(),
            model,
            messages,
            system,
            tools,
            &thinking,
        );
        let request = self
            .build_request(wire.method, &wire.url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&wire.body)?)?;

        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            let error = AgentError::from_response(response).await;
            Err(refuse_zero_quota(error, &model.id))
        }
    }
}

fn refuse_zero_quota(error: AgentError, model_id: &str) -> AgentError {
    match &error {
        AgentError::Api {
            status: TOO_MANY_REQUESTS,
            message,
            ..
        } if every_quota_limit_is_zero(message) => AgentError::api(
            PAYMENT_REQUIRED,
            format!(
                "Google gives this API key no quota for {model_id}, so retrying cannot help. Enable billing on the key's Google Cloud project, or pick a model its tier serves"
            ),
        ),
        _ => error,
    }
}

fn every_quota_limit_is_zero(body: &str) -> bool {
    let mut limits = body
        .split(QUOTA_LIMIT_LABEL)
        .skip(1)
        .map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or_default()
        })
        .peekable();
    limits.peek().is_some() && limits.all(|limit| limit == ZERO_QUOTA_LIMIT)
}

impl Provider for Google {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(self.do_stream(model, messages, system, tools, event_tx, opts.thinking))
    }

    fn wire_request(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
        _cache_key: Option<&CacheKey>,
    ) -> Result<WireRequest, AgentError> {
        Ok(self.request(
            &self.current_auth(),
            model,
            messages,
            system,
            tools,
            &opts.thinking,
        ))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        let url = self.models_url();
        let request = self.build_request("GET", &url).body(()).unwrap();
        let client = self.client.clone();
        Box::pin(async move {
            let mut response = client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            let body_text = response.text().await?;
            let models_response: ModelsListResponse = serde_json::from_str(&body_text)?;
            let mut infos: Vec<crate::model::ModelInfo> = models_response
                .models
                .into_iter()
                .filter(|m| {
                    m.supported_generation_methods
                        .iter()
                        .any(|m| m == "generateContent")
                })
                .map(|m| {
                    let id = m
                        .name
                        .strip_prefix("models/")
                        .map(String::from)
                        .unwrap_or(m.name);
                    crate::model::ModelInfo::id_only(id)
                })
                .collect();
            infos.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(infos)
        })
    }

    fn reasoning_transport(&self, _model: &Model) -> crate::ReasoningTransport {
        crate::ReasoningTransport::GeminiGenerateContent
    }
}

/// Gemini 3 and later, read from the major version in the id. An alias with no
/// version, such as `gemini-flash-latest`, points at a current model, so it is
/// treated as one. Custom models and Gemini 2.5 accept a call without a
/// signature.
fn validates_thought_signatures(model_id: &str) -> bool {
    let Some(rest) = model_id.strip_prefix(GEMINI_ID_PREFIX) else {
        return false;
    };
    match rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|major| major.parse::<u32>().ok())
    {
        Some(major) => major >= VALIDATES_SIGNATURES_SINCE,
        None => !UNVERSIONED_GEMINI_2.iter().any(|id| rest.starts_with(id)),
    }
}

/// Each model turn's first function call must carry a signature. One that has
/// none came from another model or lost it on the way, so it gets
/// [`SKIP_SIGNATURE_VALIDATOR`]. A signature Gemini issued is never replaced.
fn sign_unsigned_calls(contents: &mut [Value]) {
    for content in contents
        .iter_mut()
        .filter(|content| content["role"] == "model")
    {
        let Some(parts) = content["parts"].as_array_mut() else {
            continue;
        };
        if let Some(first_call) = parts
            .iter_mut()
            .find(|part| part.get("functionCall").is_some())
            && first_call.get("thoughtSignature").is_none()
        {
            first_call["thoughtSignature"] = json!(SKIP_SIGNATURE_VALIDATOR);
        }
    }
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let tool_names: std::collections::HashMap<&str, &str> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

    let mut out: Vec<Value> = Vec::new();

    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
        };

        let mut parts: Vec<Value> = Vec::new();
        // Gemini is strict about function-response turns: tool-returned
        // images get their own user turn instead of mixing inlineData into
        // the functionResponse content.
        let has_tool_results = msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        let mut image_parts: Vec<Value> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    parts.push(json!({"text": text}));
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                    ..
                } => {
                    let mut part = json!({"text": thinking, "thought": true});
                    if let Some(sig) = signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                ContentBlock::RedactedThinking { .. } => {}
                ContentBlock::ToolUse {
                    id: _,
                    name,
                    input,
                    thought_signature,
                } => {
                    let mut part = json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                        }
                    });
                    if let Some(sig) = thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    let parsed = serde_json::from_str::<Value>(content);
                    let mut response_val = match parsed {
                        Ok(Value::Object(map)) => Value::Object(map),
                        Ok(other) => json!({"result": other}),
                        Err(_) => json!({"result": content}),
                    };
                    if *is_error {
                        response_val = json!({"error": response_val});
                    }
                    let name = tool_names
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("unknown");
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": response_val,
                        }
                    }));
                }
                ContentBlock::Image { source } => {
                    let part = json!({
                        "inlineData": {
                            "mimeType": source.media_type.mime(),
                            "data": source.data,
                        }
                    });
                    if has_tool_results {
                        image_parts.push(part);
                    } else {
                        parts.push(part);
                    }
                }
            }
        }

        if !parts.is_empty() {
            out.push(json!({"role": role, "parts": parts}));
        }
        if !image_parts.is_empty() {
            out.push(json!({"role": "user", "parts": image_parts}));
        }
    }

    out
}

/// Tool schemas go out as `parametersJsonSchema`, which takes standard JSON
/// Schema. The older `parameters` field takes an OpenAPI subset and refuses the
/// whole request over `$schema`, `$ref`, `const` and other keywords that
/// Workcell and MCP tools declare.
fn convert_tools(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let description = t.get("description")?.as_str().unwrap_or("");
            let schema = match t.get("input_schema") {
                Some(Value::Object(schema)) => json_schema::normalize(schema.clone()),
                _ => json!({"type": "object", "properties": {}}),
            };
            Some(json!({
                "name": name,
                "description": description,
                "parametersJsonSchema": schema,
            }))
        })
        .collect()
}

fn tool_defs_empty(tool_decls: &[Value]) -> bool {
    tool_decls.is_empty()
}

// --- SSE response types ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseCandidate {
    content: Option<SseContent>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseContent {
    parts: Option<Vec<SsePart>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsePart {
    text: Option<String>,
    thought: Option<bool>,
    thought_signature: Option<String>,
    function_call: Option<SseFunctionCall>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseFunctionCall {
    name: String,
    args: Option<Value>,
    #[serde(default)]
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseUsageMetadata {
    /// The whole prompt, cached tokens included.
    #[serde(default)]
    prompt_token_count: u32,
    /// The reply alone. Thinking is counted apart and bills as output too.
    #[serde(default)]
    candidates_token_count: u32,
    #[serde(default)]
    cached_content_token_count: u32,
    #[serde(default)]
    thoughts_token_count: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseResponse {
    candidates: Option<Vec<SseCandidate>>,
    usage_metadata: Option<SseUsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsListResponse {
    models: Vec<ApiModelInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiModelInfo {
    name: String,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

/// Append `text` to the last block when it is already a `Text`, else push a new
/// `Text` block. Gemini streams a single assistant message as many small text
/// parts across SSE chunks; coalescing keeps them as one content block so
/// session restore renders one `caudra>` block instead of one per delta.
fn push_or_extend_text(blocks: &mut Vec<ContentBlock>, text: String) {
    if let Some(ContentBlock::Text { text: prev }) = blocks.last_mut() {
        prev.push_str(&text);
    } else {
        blocks.push(ContentBlock::Text { text });
    }
}

/// Same as `push_or_extend_text` for thinking parts. The `thoughtSignature`
/// arrives on the final thinking delta, so a later signature overwrites an
/// earlier one; a `Some` on an earlier delta is preserved if the last is None.
fn push_or_extend_thinking(
    blocks: &mut Vec<ContentBlock>,
    text: String,
    signature: Option<String>,
) {
    if let Some(ContentBlock::Thinking {
        thinking: prev,
        signature: prev_sig,
        ..
    }) = blocks.last_mut()
    {
        prev.push_str(&text);
        if signature.is_some() {
            *prev_sig = signature;
        }
    } else {
        blocks.push(ContentBlock::thinking(text, signature));
    }
}

async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            _ => continue,
        };

        let chunk: SseResponse = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to parse Gemini SSE chunk");
                continue;
            }
        };

        if let Some(meta) = chunk.usage_metadata {
            usage.input = meta
                .prompt_token_count
                .saturating_sub(meta.cached_content_token_count);
            usage.cache_read = meta.cached_content_token_count;
            usage.output = meta
                .candidates_token_count
                .saturating_add(meta.thoughts_token_count);
        }

        let Some(candidates) = chunk.candidates else {
            continue;
        };

        for candidate in candidates {
            if let Some(reason) = candidate.finish_reason {
                stop_reason = Some(StopReason::from_google(&reason)).or(stop_reason);
            }

            let Some(content) = candidate.content else {
                continue;
            };
            let Some(parts) = content.parts else {
                continue;
            };

            for part in parts {
                if let Some(func_call) = part.function_call {
                    let id = format!("call_{}_{}", func_call.name, CaudraId::generate());
                    let input = func_call.args.unwrap_or_default();
                    let thought_signature = func_call.thought_signature.or(part.thought_signature);
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: id.clone(),
                            name: func_call.name.clone(),
                            source_ordinal: None,
                        })
                        .await?;
                    // Google has no argument deltas: the whole call arrives at
                    // once. One delta carrying all of it keeps every provider
                    // looking the same downstream.
                    if input.is_object() {
                        event_tx
                            .send_async(ProviderEvent::ToolInputDelta {
                                id: id.clone(),
                                delta: input.to_string(),
                            })
                            .await?;
                    }
                    event_tx
                        .send_async(ProviderEvent::ToolInputReady {
                            id: id.clone(),
                            name: func_call.name.clone(),
                            input: input.clone(),
                            invalid_input: None,
                        })
                        .await?;
                    content_blocks.push(ContentBlock::ToolUse {
                        id,
                        name: func_call.name,
                        input,
                        thought_signature,
                    });
                    stop_reason = Some(StopReason::ToolUse);
                } else if let Some(text) = part.text {
                    if part.thought.unwrap_or(false) {
                        if !text.is_empty() {
                            event_tx
                                .send_async(ProviderEvent::ThinkingDelta { text: text.clone() })
                                .await?;
                        }
                        push_or_extend_thinking(&mut content_blocks, text, part.thought_signature);
                    } else if !text.is_empty() {
                        // TODO: preserve part.thought_signature if ContentBlock::Text adds signature support.
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: text.clone() })
                            .await?;
                        push_or_extend_text(&mut content_blocks, text);
                    }
                }
            }
        }
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_support::{
        CREDENTIAL_IN_URL, PEER_ATTACK, PEER_TEXT, assert_peer_framing, automation_event_origin,
        peer_message_origin, task_event_origin, task_observation_with_output_refs,
        workflow_event_origin,
    };
    use crate::{ReasoningOptions, StandingReminderKind, SteeringKind};
    use std::sync::Arc;
    use test_case::test_case;

    const GEMINI_API_KEY: &str = "test-key";
    const STEERING_TEXT: &str = "Continue with a useful response.";
    const STEERING_RULE: &str = "empty_output";
    const TOOL_NAME: &str = "read";
    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(30);

    #[test_case(r#"{"INVALID_JSON":"display","caudra_invalid_json_raw":"{\"command\":\"embedded\"}","caudra_invalid_json_complete":true,"caudra_invalid_json_clipped":false,"command":"actual"}"# ; "spoofed_metadata")]
    fn valid_marker_fields_survive_wire_projection(raw: &str) {
        let input: Value = serde_json::from_str(raw).unwrap();
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(TOOL_NAME, TOOL_NAME, input.clone())],
            ..Message::default()
        };
        let wire = convert_messages(&[message]);
        assert_eq!(wire[0]["parts"][0]["functionCall"]["args"], input);
    }

    #[test_case(json!({"name": TOOL_NAME}), Value::Null ; "omitted_arguments")]
    #[test_case(json!({"name": TOOL_NAME, "args": {}}), json!({}) ; "empty_object")]
    fn parse_sse_preserves_argument_semantics(call: Value, expected: Value) {
        let chunk = json!({"candidates": [{"content": {"parts": [{"functionCall": call}]}, "finishReason": "STOP"}]});
        let body = isahc::AsyncBody::from(format!("data: {chunk}\n\n"));
        let response = isahc::Response::builder().status(200).body(body).unwrap();
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, TEST_STREAM_TIMEOUT)).unwrap();
        let tools: Vec<_> = result.message.tool_uses().collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, TOOL_NAME);
        assert_eq!(tools[0].2, &expected);
    }

    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Recovery) ; "recovery")]
    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Advisory) ; "advisory")]
    #[test_case(Message::task_observation(STEERING_TEXT.into(), task_event_origin()) ; "task_event")]
    #[test_case(task_observation_with_output_refs(STEERING_TEXT); "retained_task_outputs")]
    #[test_case(Message::workflow_observation(STEERING_TEXT.into(), workflow_event_origin()) ; "workflow_event")]
    #[test_case(Message::automation_observation(STEERING_TEXT.into(), automation_event_origin()) ; "automation_event")]
    #[test_case(Message::standing_reminder(STEERING_TEXT.into(), StandingReminderKind::BackgroundWork) ; "background_reminder")]
    fn observation_metadata_is_not_on_wire(message: Message) {
        let wire = convert_messages(&[message]);
        assert_eq!(
            wire,
            vec![json!({"role": "user", "parts": [{"text": STEERING_TEXT}]})]
        );
    }

    #[test_case(PEER_TEXT ; "plain_text")]
    #[test_case(PEER_ATTACK ; "adversarial_host_markers")]
    fn peer_observation_wire_contains_only_framed_text(text: &str) {
        let origin = peer_message_origin();
        let wire = convert_messages(&[Message::peer_observation(text.into(), origin.clone())]);
        let framed = wire[0]["parts"][0]["text"].as_str().unwrap();
        assert_peer_framing(framed, text, &origin);
        assert_eq!(
            wire,
            vec![json!({"role": "user", "parts": [{"text": framed}]})]
        );
    }

    fn test_auth() -> Arc<Mutex<ResolvedAuth>> {
        Arc::new(Mutex::new(ResolvedAuth {
            base_url: None,
            headers: vec![("x-goog-api-key".into(), GEMINI_API_KEY.into())],
        }))
    }

    fn test_timeouts() -> super::super::Timeouts {
        super::super::Timeouts {
            connect: Duration::from_secs(5),
            stream: Duration::from_secs(300),
        }
    }

    fn test_model() -> Model {
        Model {
            id: "gemini-2.5-flash".into(),
            provider: Arc::<str>::from("google"),
            family: ModelFamily::Gemini,
            supports_vision_override: Some(true),
            supports_cache_breakpoints_override: None,
            supports_pdf: false,
            supports_tool_examples_override: None,
            thinking_override: None,
            pricing: ModelPricing::default(),
            price_change: None,
            discovered_free: false,
            max_output_tokens: Some(8192),
            context_window: 1_048_576,
            window_excludes_output: false,
            reasoning_options: ReasoningOptions::default(),
            thinking_fields: None,
            billing: crate::model::Billing::default(),
        }
    }

    fn dry_run(messages: &[Message], system: &str, thinking: ThinkingConfig) -> WireRequest {
        Google::with_auth(test_auth(), test_timeouts())
            .wire_request(
                &test_model(),
                messages,
                system,
                &json!([]),
                &RequestOptions {
                    thinking,
                    ..RequestOptions::default()
                },
                None,
            )
            .unwrap()
    }

    #[test]
    fn google_build_body_basic() {
        let body = dry_run(
            &[Message::user("hello".into())],
            "be helpful",
            ThinkingConfig::Off,
        )
        .body;

        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn google_build_body_thinking_adaptive() {
        let body = dry_run(
            &[Message::user("think".into())],
            "",
            ThinkingConfig::Adaptive,
        )
        .body;

        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn google_build_body_thinking_budget() {
        let body = dry_run(
            &[Message::user("think hard".into())],
            "",
            ThinkingConfig::Budget(8192),
        )
        .body;

        // Clamped to the model's max thinking budget (half of 8192 output tokens).
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
    }

    #[test]
    fn wire_request_streams_without_the_key_in_the_url() {
        let wire = dry_run(&[], "", ThinkingConfig::Off);

        assert!(wire.url.starts_with(BASE_URL));
        assert!(!wire.url.contains(GEMINI_API_KEY), "{CREDENTIAL_IN_URL}");
    }

    #[test_case("STOP", StopReason::EndTurn ; "stop")]
    #[test_case("MAX_TOKENS", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("CONTINUATION", StopReason::MaxTokens ; "long_decode_stopped_mid_answer")]
    #[test_case("SAFETY", StopReason::Refusal ; "safety")]
    #[test_case("RECITATION", StopReason::Refusal ; "recitation")]
    #[test_case("PROHIBITED_CONTENT", StopReason::Refusal ; "prohibited_content")]
    #[test_case("unknown", StopReason::EndTurn ; "unknown")]
    fn stop_reason_from_google(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_google(input), expected);
    }

    const ARGON_SPEC: &str = "google/gemini-4-argon";
    const FLASH_3_6_SPEC: &str = "google/gemini-3.6-flash";
    const FLASH_LITE_SPEC: &str = "google/gemini-3.5-flash-lite";
    const FLASH_2_5_SPEC: &str = "google/gemini-2.5-flash";
    const ISSUED_SIGNATURE: &str = "sig-from-gemini";
    const ONLY_THE_FIRST_CALL: &str = "Gemini validates only the first call of a step";

    fn effort(level: &str) -> ThinkingConfig {
        ThinkingConfig::Effort(level.into())
    }

    fn table_body(spec: &str, messages: &[Message], thinking: ThinkingConfig) -> Value {
        declaring_body(spec, messages, thinking, &json!([]))
    }

    fn declaring_body(
        spec: &str,
        messages: &[Message],
        thinking: ThinkingConfig,
        tools: &Value,
    ) -> Value {
        let model = Model::from_spec(spec).unwrap();
        Google::with_auth(test_auth(), test_timeouts())
            .wire_request(
                &model,
                messages,
                "",
                tools,
                &RequestOptions {
                    thinking,
                    ..RequestOptions::default()
                },
                None,
            )
            .unwrap()
            .body
    }

    /// Gemini 4 and 3 cannot stop thinking, so `off` asks for the shallowest
    /// level each model accepts, and a level it would reject snaps into range.
    #[test_case(ARGON_SPEC, ThinkingConfig::Adaptive, json!({"includeThoughts": true})                         ; "argon_adaptive_leaves_the_level_to_gemini")]
    #[test_case(ARGON_SPEC, ThinkingConfig::Off, json!({"includeThoughts": true, "thinkingLevel": "low"})      ; "argon_off_runs_at_low")]
    #[test_case(ARGON_SPEC, effort("minimal"), json!({"includeThoughts": true, "thinkingLevel": "low"})        ; "argon_rejects_minimal")]
    #[test_case(ARGON_SPEC, effort("max"), json!({"includeThoughts": true, "thinkingLevel": "high"})           ; "argon_tops_out_at_high")]
    #[test_case(FLASH_LITE_SPEC, ThinkingConfig::Off, json!({"includeThoughts": true, "thinkingLevel": "minimal"}) ; "flash_lite_off_runs_at_minimal")]
    #[test_case(FLASH_3_6_SPEC, effort("minimal"), json!({"includeThoughts": true, "thinkingLevel": "minimal"}) ; "flash_3_6_keeps_minimal")]
    fn gemini_thinking_levels_reach_the_wire(
        spec: &str,
        thinking: ThinkingConfig,
        expected: Value,
    ) {
        let body = table_body(spec, &[Message::user("hi".into())], thinking);
        assert_eq!(body["generationConfig"]["thinkingConfig"], expected);
    }

    #[test]
    fn argon_asks_for_no_more_than_one_streaming_request_returns() {
        let body = table_body(ARGON_SPEC, &[], ThinkingConfig::Adaptive);
        assert_eq!(
            body["generationConfig"]["maxOutputTokens"],
            MAX_OUTPUT_TOKENS
        );
    }

    fn parallel_calls(first_signature: Option<&str>) -> Vec<Message> {
        let call = |id: &str, thought_signature: Option<&str>| ContentBlock::ToolUse {
            id: id.into(),
            name: TOOL_NAME.into(),
            input: json!({}),
            thought_signature: thought_signature.map(Into::into),
        };
        vec![
            Message::user("read both".into()),
            Message {
                role: Role::Assistant,
                content: vec![call("call_1", first_signature), call("call_2", None)],
                ..Default::default()
            },
        ]
    }

    #[test_case(ARGON_SPEC, None, Some(SKIP_SIGNATURE_VALIDATOR)            ; "argon_signs_a_call_from_another_model")]
    #[test_case(FLASH_3_6_SPEC, None, Some(SKIP_SIGNATURE_VALIDATOR)        ; "gemini_3_signs_a_call_from_another_model")]
    #[test_case(ARGON_SPEC, Some(ISSUED_SIGNATURE), Some(ISSUED_SIGNATURE)  ; "an_issued_signature_is_kept")]
    #[test_case(FLASH_2_5_SPEC, None, None                                   ; "gemini_2_5_takes_unsigned_calls")]
    fn the_first_call_of_a_model_turn_carries_a_signature(
        spec: &str,
        signature: Option<&str>,
        expected: Option<&str>,
    ) {
        let body = table_body(spec, &parallel_calls(signature), ThinkingConfig::Adaptive);
        let parts = &body["contents"][1]["parts"];
        assert_eq!(parts[0]["thoughtSignature"].as_str(), expected);
        assert!(
            parts[1].get("thoughtSignature").is_none(),
            "{ONLY_THE_FIRST_CALL}"
        );
    }

    const LEGACY_MCP_CALL: &str = "everything__get-sum";
    const CODE_MODE_TOOL: &str = "code";
    const LEGACY_RESULT: &str = "The sum of 4 and 5 is 9.";

    /// A session from before MCP code mode called each MCP tool directly. Resumed now, it
    /// declares only `code`, and the old call still goes out under its own name, signed for the
    /// models that check, with its response paired to it.
    #[test_case(ARGON_SPEC     ; "gemini_4")]
    #[test_case(FLASH_3_6_SPEC ; "gemini_3")]
    #[test_case(FLASH_2_5_SPEC ; "gemini_2_5")]
    fn a_direct_mcp_call_from_before_code_mode_is_replayed_as_it_was(spec: &str) {
        let arguments = json!({"a": 4, "b": 5});
        let history = [
            Message::user("add 4 and 5".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    LEGACY_MCP_CALL,
                    arguments.clone(),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: LEGACY_RESULT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message::user("and again".into()),
        ];
        let tools = json!([{
            "name": CODE_MODE_TOOL,
            "description": "Run Python that calls MCP servers.",
            "input_schema": {"type": "object", "properties": {"code": {"type": "string"}}},
        }]);

        let body = declaring_body(spec, &history, ThinkingConfig::Adaptive, &tools);

        let call = &body["contents"][1]["parts"][0];
        assert_eq!(call["functionCall"]["name"], LEGACY_MCP_CALL);
        assert_eq!(call["functionCall"]["args"], arguments);
        let signed = validates_thought_signatures(&Model::from_spec(spec).unwrap().id);
        assert_eq!(
            call["thoughtSignature"].as_str(),
            signed.then_some(SKIP_SIGNATURE_VALIDATOR)
        );
        let response = &body["contents"][2]["parts"][0]["functionResponse"];
        assert_eq!(response["name"], LEGACY_MCP_CALL);
        assert_eq!(response["response"], json!({"result": LEGACY_RESULT}));
        assert_eq!(
            body["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|declaration| &declaration["name"])
                .collect::<Vec<_>>(),
            [CODE_MODE_TOOL]
        );
    }

    #[test_case("gemini-4-argon", true         ; "gemini_4")]
    #[test_case("gemini-3.5-flash-lite", true  ; "gemini_3")]
    #[test_case("gemini-10-pro", true          ; "two_digit_major")]
    #[test_case("gemini-2.5-pro", false        ; "gemini_2_5")]
    #[test_case("gemini-flash-latest", true    ; "an_alias_follows_the_current_model")]
    #[test_case("gemini-robotics-er-1.5-preview", false ; "an_unversioned_gemini_2_5")]
    #[test_case("gemma-3-27b-it", false        ; "not_gemini")]
    fn signatures_are_validated_from_gemini_3(model_id: &str, expected: bool) {
        assert_eq!(validates_thought_signatures(model_id), expected);
    }

    #[test]
    fn convert_messages_user_and_assistant() {
        let messages = vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["parts"][0]["text"], "hello");
        assert_eq!(result[1]["role"], "model");
        assert_eq!(result[1]["parts"][0]["text"], "hi there");
    }

    #[test]
    fn convert_messages_thinking_block() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::thinking("hmm".into(), Some("sig123".into()))],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["text"], "hmm");
        assert_eq!(result[0]["parts"][0]["thought"], true);
        assert_eq!(result[0]["parts"][0]["thoughtSignature"], "sig123");
    }

    #[test]
    fn convert_messages_tool_use_and_result() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    "read_file",
                    json!({"path": "/tmp/a"}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "file contents".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
    }

    #[test_case("not json at all", json!({"result": "not json at all"}) ; "non_json_wraps_string")]
    #[test_case(r#""a json string""#, json!({"result": "a json string"}) ; "json_scalar_wraps")]
    #[test_case("42", json!({"result": 42}) ; "json_number_wraps")]
    #[test_case(r#"{"out": "ok"}"#, json!({"out": "ok"}) ; "json_object_passes_through")]
    fn convert_messages_tool_result_response_is_always_struct(content: &str, expected: Value) {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call_1", "read", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: content.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["response"],
            expected
        );
    }

    #[test]
    fn convert_messages_tool_result_error_wraps_response() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call_1", "read", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "boom".into(),
                    is_error: true,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["response"],
            json!({"error": {"result": "boom"}})
        );
    }

    #[test]
    fn convert_messages_tool_use_preserves_thought_signature() {
        const SIG: &str = "sig-abc";
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({"path": "/tmp/a"}),
                thought_signature: Some(SIG.into()),
            }],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(result[0]["parts"][0]["thoughtSignature"], SIG);
        assert!(
            result[0]["parts"][0]["functionCall"]
                .get("thoughtSignature")
                .is_none()
        );
    }

    #[test]
    fn convert_messages_tool_returned_image_gets_own_user_turn() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Png,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 2);
        assert!(result[0]["parts"][0].get("functionResponse").is_some());
        assert!(result[0]["parts"].as_array().unwrap().len() == 1);
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["parts"][0]["inlineData"]["mimeType"], "image/png");
    }

    #[test]
    fn convert_messages_chat_pasted_image_stays_in_same_turn() {
        // Split the turn and the question drifts apart from its picture.
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Webp,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 1);
        let parts = result[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/webp");
    }

    #[test_case(
        json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd", "ghost"], "additionalProperties": false}),
        json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"], "additionalProperties": false})
        ; "a_declared_schema_is_normalized"
    )]
    #[test_case(Value::Null, json!({"type": "object", "properties": {}}) ; "a_missing_schema_takes_no_arguments")]
    fn tools_declare_standard_json_schema(input_schema: Value, expected: Value) {
        let tools =
            json!([{"name": TOOL_NAME, "description": TOOL_NAME, "input_schema": input_schema}]);
        let declarations = convert_tools(&tools);
        assert_eq!(declarations[0]["parametersJsonSchema"], expected);
        assert!(declarations[0].get("parameters").is_none());
    }

    #[test]
    fn models_list_has_defaults() {
        let models = models();
        assert!(!models.is_empty());
        for entry in models {
            assert!(!entry.prefixes.is_empty());
            assert!(entry.max_output_tokens.is_some_and(|t| t > 0));
            assert!(entry.context_window >= entry.max_output_tokens.unwrap());
        }
    }

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_plain_text() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":10}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(result.usage.input, 5);
        assert_eq!(result.usage.output, 10);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "hello"
        ));
    }

    #[test]
    fn parse_sse_coalesces_text_deltas_into_one_block() {
        let event = |text: &str| {
            format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\"}}]}}}}]}}\n\n",
                text
            )
        };
        let mut data = String::new();
        data.push_str(&event("Hello, "));
        data.push_str(&event("world."));
        let response = mock_response(data.leak().as_bytes());
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(
            result.message.content.len(),
            1,
            "expected one coalesced text block"
        );
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "Hello, world."
        ));
    }

    #[test]
    fn parse_sse_coalesces_thinking_deltas_keeps_last_signature() {
        let event = |text: &str, sig: Option<&str>| match sig {
            Some(s) => format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\",\"thought\":true,\"thoughtSignature\":\"{}\"}}]}}}}]}}\n\n",
                text, s
            ),
            None => format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\",\"thought\":true}}]}}}}]}}\n\n",
                text
            ),
        };
        let mut data = String::new();
        data.push_str(&event("reasoning... ", None));
        data.push_str(&event("more.", Some("sig-final")));
        let response = mock_response(data.leak().as_bytes());
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, signature, .. }
                if thinking == "reasoning... more." && signature.as_deref() == Some("sig-final")
        ));
    }

    #[test]
    fn parse_sse_parallel_same_name_tool_calls_get_unique_ids() {
        let part = r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"}}}"#;
        let payload = format!(r#"{{"candidates":[{{"content":{{"parts":[{part},{part}]}}}}]}}"#,);
        let data = format!("data: {payload}\n\n");
        let response = mock_response(data.leak().as_bytes());
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        let ids: Vec<&str> = result
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "expected two tool calls");
        assert_ne!(ids[0], ids[1], "ids must be unique for parallel calls");
    }

    #[test]
    fn parse_sse_thinking_part() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"thinking...\",\"thought\":true,\"thoughtSignature\":\"sig1\"}]}},{\"content\":{\"parts\":[{\"text\":\"answer\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":20}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, signature, .. } if thinking == "thinking..." && signature.as_deref() == Some("sig1")
        ));
        assert!(matches!(
            &result.message.content[1],
            ContentBlock::Text { text } if text == "answer"
        ));
    }

    #[test]
    fn parse_sse_tool_call() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"cmd\":\"ls\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":15}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::ToolUse { name, .. } if name == "bash"
        ));
    }

    #[test_case(
        r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"}},"thoughtSignature":"CvcQAdHtim/pKv/c0ClPFkYA=="}"#
        ; "part_level"
    )]
    #[test_case(
        r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"},"thoughtSignature":"CvcQAdHtim/pKv/c0ClPFkYA=="}}"#
        ; "nested_fallback"
    )]
    fn parse_sse_tool_call_captures_thought_signature(part_json: &str) {
        const SIG: &str = "CvcQAdHtim/pKv/c0ClPFkYA==";
        let payload = format!(
            r#"{{"candidates":[{{"content":{{"parts":[{part_json}]}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":5,"candidatesTokenCount":15}}}}"#,
        );
        let data = format!("data: {payload}\n\n");
        let response = mock_response(data.leak().as_bytes());
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::ToolUse { thought_signature: Some(s), .. } if s == SIG
        ));
    }

    const CACHED_ONCE: &str = "promptTokenCount already includes the cached tokens";
    const THINKING_IS_OUTPUT: &str = "Gemini bills thinking tokens as output";
    const ZERO_QUOTA_BODY: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_requests, limit: 0, model: gemini-4-argon\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_input_token_count, limit: 0, model: gemini-4-argon\nPlease retry in 13h3m20s.","status":"RESOURCE_EXHAUSTED"}}"#;
    const SPENT_QUOTA_BODY: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota.\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_requests, limit: 15, model: gemini-3.8-flash\nPlease retry in 40s.","status":"RESOURCE_EXHAUSTED"}}"#;
    const ONE_LIVE_QUOTA_BODY: &str = r#"{"error":{"code":429,"message":"* Quota exceeded for metric: a, limit: 0, model: gemini-4-argon\n* Quota exceeded for metric: b, limit: 250000, model: gemini-4-argon","status":"RESOURCE_EXHAUSTED"}}"#;
    const UNNAMED_QUOTA_BODY: &str = r#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#;
    const ZERO_NEVER_REFILLS: &str = "a quota of 0 never refills, so waiting cannot help";

    #[test]
    fn parse_sse_cached_tokens() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":50}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.usage.input, 50, "{CACHED_ONCE}");
        assert_eq!(result.usage.cache_read, 50, "{CACHED_ONCE}");
        assert_eq!(result.usage.total_input(), 100, "{CACHED_ONCE}");
        assert_eq!(result.usage.output, 10);
    }

    #[test]
    fn thinking_tokens_bill_as_output() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"thoughtsTokenCount\":30}}\n\n";
        let response = mock_response(data);
        let (tx, _rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(30))).unwrap();
        assert_eq!(result.usage.output, 40, "{THINKING_IS_OUTPUT}");
        assert_eq!(result.usage.input, 100);
    }

    #[test_case(ZERO_QUOTA_BODY, false     ; "every_limit_at_zero_is_a_billing_refusal")]
    #[test_case(SPENT_QUOTA_BODY, true     ; "a_spent_limit_refills")]
    #[test_case(ONE_LIVE_QUOTA_BODY, true  ; "one_live_limit_is_worth_the_wait")]
    #[test_case(UNNAMED_QUOTA_BODY, true   ; "no_limit_named")]
    fn only_a_quota_of_zero_stops_retrying(body: &str, retryable: bool) {
        let error = refuse_zero_quota(AgentError::api(TOO_MANY_REQUESTS, body), ARGON);
        assert_eq!(error.is_retryable(), retryable, "{ZERO_NEVER_REFILLS}");
    }

    #[test]
    fn a_zero_quota_names_the_model() {
        let error = refuse_zero_quota(AgentError::api(TOO_MANY_REQUESTS, ZERO_QUOTA_BODY), ARGON);
        assert_eq!(error.status(), Some(PAYMENT_REQUIRED));
        assert!(error.user_message().contains(ARGON));
    }
}
