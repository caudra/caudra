use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::sync::{Arc, LazyLock};

use crate::types::{append_tool_input, parse_tool_input};
use flume::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::model::{
    ANTHROPIC_SLUG, FastPricing, Model, ModelEntry, ModelFamily, ModelPricing,
    StaticReasoningOption,
};
use crate::{
    AgentError, ContentBlock, EMPTY_RESPONSE_MARKER, InvalidToolInput, Message, ProviderEvent,
    Role, StopReason, StreamResponse, ThinkingConfig, TokenUsage,
};

pub(super) const BETA_TOOL_EXAMPLES_BEDROCK: &str = "tool-examples-2025-10-29";

/// Tool names that are also ordinary English words. Prose rewriting only
/// touches these when they are fenced in backticks or bold, so "a shell command
/// pipeline" survives while `` `shell` `` is renamed with everything else.
const AMBIGUOUS_TOOL_WORDS: &[&str] = &[
    "batch", "index", "memory", "question", "shell", "skill", "task", "workflow",
];

/// Longest wire name the messages API accepts.
const MAX_TOOL_NAME: usize = 64;

/// The messages API refuses requests without max_tokens. Anthropic-kind
/// models always get a window from the fallback table, so this only fires if
/// an unknown-window model is ever routed here; 32k is safe for every Claude.
pub(crate) const FALLBACK_MAX_TOKENS: u32 = 32_000;

/// A `-1m` suffix is our own convention for asking Anthropic for the full 1M
/// context window. The API has never heard of the suffix, so we strip it from
/// the id before sending.
pub(crate) const LONG_CONTEXT_SUFFIX: &str = "-1m";
pub(crate) const LONG_CONTEXT_WINDOW: u32 = 1_000_000;

/// Long-context models accept 1M tokens natively, with no beta header. This is
/// the working window we run them at, capped well below that ceiling to bound
/// cost and latency, and aligned with `WIDE_PLAN_CONTEXT_WINDOW`. Unlike the
/// 200k entries, it is an *input* budget: `max_output_tokens` is granted on top
/// of it rather than carved out of it.
pub(crate) const WIDE_CONTEXT_WINDOW: u32 = 372_000;

const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &str = "59cf53e54c78";
const TEXT_BLOCK: &str = "text";
const THINKING_DROPPED: &str = "thinking_dropped";

pub(crate) fn strip_long_context(model_id: &str) -> &str {
    model_id
        .strip_suffix(LONG_CONTEXT_SUFFIX)
        .unwrap_or(model_id)
}

/// A `-1m` model is just its base entry with a wider window.
pub(crate) fn long_context_window(model_id: &str) -> Option<u32> {
    model_id
        .ends_with(LONG_CONTEXT_SUFFIX)
        .then_some(LONG_CONTEXT_WINDOW)
}

/// Whether a static entry's own declared window is an input budget rather than
/// the API total. Only the window caudra caps itself is one, and only this table
/// declares any, so the question is scoped to an entry the manifest owns: a
/// window discovered from a provider or published by models.dev answers for
/// itself and never reaches here.
pub(crate) fn declares_input_budget(manifest_slug: &str, entry: &ModelEntry) -> bool {
    manifest_slug == ANTHROPIC_SLUG && entry.context_window == WIDE_CONTEXT_WINDOW
}

pub(super) const MESSAGE_CACHE_BREAKPOINTS: usize = 2;

static EMPTY_CONTENT: LazyLock<ContentBlock> = LazyLock::new(|| ContentBlock::Text {
    text: EMPTY_RESPONSE_MARKER.into(),
});

#[derive(Serialize)]
pub(crate) struct CacheControl {
    pub r#type: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl {
    r#type: "ephemeral",
};

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

impl From<Usage> for TokenUsage {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }
    }
}

/// A change the API made to the request before the model read it. Later
/// checks add kinds and reasons, so both stay open strings.
#[derive(Deserialize, Default)]
#[serde(default)]
struct InputTransformation {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    reason: String,
}

#[derive(Deserialize)]
struct MessagePayload {
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    input_transformations: Option<Vec<InputTransformation>>,
}

/// Each dropped block is reasoning the model answered without, whether a
/// prefix edit, a model switch or another account invalidated it.
fn warn_dropped_thinking(transformations: &[InputTransformation]) {
    let dropped: Vec<&InputTransformation> = transformations
        .iter()
        .filter(|transformation| transformation.kind == THINKING_DROPPED)
        .collect();
    let Some(first) = dropped.first() else {
        return;
    };
    let reasons: BTreeSet<&str> = dropped
        .iter()
        .map(|transformation| transformation.reason.as_str())
        .collect();
    warn!(
        dropped = dropped.len(),
        first_path = %first.path,
        ?reasons,
        "the API dropped thinking blocks before the model read them"
    );
}

#[derive(Deserialize)]
struct MessageStartEvent {
    message: MessagePayload,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseContentBlock {
    Text,
    Thinking,
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Deserialize)]
struct ContentBlockDeltaEvent {
    index: usize,
    delta: Delta,
}

#[derive(Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaEvent {
    #[serde(default)]
    delta: Option<MessageDeltaPayload>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(super) struct WireContentBlock<'a> {
    #[serde(flatten)]
    pub inner: WireContent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

pub(super) struct WireContent<'a>(&'a ContentBlock);

impl Serialize for WireContent<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct ToolResult<'a> {
            r#type: &'static str,
            tool_use_id: &'a str,
            content: &'a str,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            is_error: bool,
        }

        match self.0 {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => ToolResult {
                r#type: "tool_result",
                tool_use_id,
                content,
                is_error: *is_error,
            }
            .serialize(serializer),
            block => block.serialize(serializer),
        }
    }
}

#[derive(Serialize)]
pub(super) struct WireMessage<'a> {
    pub role: &'a Role,
    pub content: Vec<WireContentBlock<'a>>,
}

/// The API rejects blank text blocks, and messages with no block at all, so
/// blanks go and a message left bare falls back to the marker.
fn wire_content(msg: &Message) -> Vec<WireContentBlock<'_>> {
    let mut content: Vec<WireContentBlock<'_>> = msg
        .content
        .iter()
        .filter(|block| !matches!(block, ContentBlock::Text { text } if text.trim().is_empty()))
        .map(|inner| WireContentBlock {
            inner: WireContent(inner),
            cache_control: None,
        })
        .collect();

    if content.is_empty() {
        content.push(WireContentBlock {
            inner: WireContent(&EMPTY_CONTENT),
            cache_control: None,
        });
    }
    content
}

pub(super) fn build_wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    let len = messages.len();

    messages
        .iter()
        .enumerate()
        .map(|(msg_idx, msg)| {
            let mut content = wire_content(msg);

            // The API rejects `cache_control` on thinking blocks, so walk back to
            // the last block that can carry it. All thinking means no breakpoint,
            // which beats a fatal one.
            if msg_idx + MESSAGE_CACHE_BREAKPOINTS >= len
                && let Some(block) = content.iter_mut().rfind(|b| !b.inner.0.is_thinking())
            {
                block.cache_control = Some(EPHEMERAL);
            }

            WireMessage {
                role: &msg.role,
                content,
            }
        })
        .collect()
}

/// Without this the API buffers and validates each parameter value before
/// streaming it back, so a large argument such as a written file body arrives
/// as one fragment at `content_block_stop` and the UI cannot draw it growing.
/// The per-tool field supersedes the `fine-grained-tool-streaming-2025-05-14`
/// beta header, which the API rejects alongside computer-use toolset entries.
const EAGER_INPUT_STREAMING: &str = "eager_input_streaming";

pub(super) fn build_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr.to_vec();
    for tool in &mut out {
        tool[EAGER_INPUT_STREAMING] = json!(true);
    }
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

/// The prompt's breakpoint caches a prefix ahead of it too, so the prefix
/// spends none of the four the API allows.
fn system_blocks<'a>(prefix: Option<&'a str>, system: &'a str) -> Vec<SystemBlock<'a>> {
    let prompt = SystemBlock {
        r#type: TEXT_BLOCK,
        text: system,
        cache_control: Some(EPHEMERAL),
    };
    match prefix {
        Some(prefix) => vec![
            SystemBlock {
                r#type: TEXT_BLOCK,
                text: prefix,
                cache_control: None,
            },
            prompt,
        ],
        None => vec![prompt],
    }
}

/// The body every Anthropic-protocol endpoint shares. Bedrock names the model
/// in its URL, so it builds on this rather than on [`messages_body`].
pub(crate) fn request_body(
    model: &Model,
    messages: &[Message],
    system_prefix: Option<&str>,
    system: &str,
    tools: &Value,
    thinking: &ThinkingConfig,
) -> Value {
    let mut body = json!({
        "max_tokens": model.max_output_tokens.unwrap_or(FALLBACK_MAX_TOKENS),
        "system": system_blocks(system_prefix, system),
        "messages": build_wire_messages(messages),
        "tools": build_wire_tools(tools),
    });

    thinking.apply_to_body(&mut body, model);
    body
}

/// [`request_body`] as the Messages API takes it: naming its model and asking
/// for a stream.
pub(crate) fn messages_body(
    model: &Model,
    model_id: &str,
    messages: &[Message],
    system_prefix: Option<&str>,
    system: &str,
    tools: &Value,
    thinking: &ThinkingConfig,
) -> Value {
    let mut body = request_body(model, messages, system_prefix, system, tools, thinking);
    body["model"] = json!(model_id);
    body["stream"] = json!(true);
    body
}

pub(super) fn apply_oauth_request_profile(
    body: &mut Value,
    system: &str,
    version: &str,
) -> HashMap<String, String> {
    let first_user_text = first_user_text(body).unwrap_or_default();
    let billing = billing_header(first_user_text, version);
    body["system"] = json!([
        {"type": "text", "text": billing},
        {"type": "text", "text": CLAUDE_CODE_IDENTITY},
    ]);

    // Every tool is registered before any prose is rewritten: a description may
    // name a sibling declared after it, and the system prompt names tools in no
    // particular order.
    let mut renames = HashMap::new();
    if let Some(tools) = body["tools"].as_array() {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                mapped_oauth_tool_name(name, &mut renames);
            }
        }
    }

    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            if let Some(wire) = tool["name"]
                .as_str()
                .and_then(|name| renames.get(name))
                .cloned()
            {
                tool["name"] = json!(wire);
            }
            // Descriptions cross-reference siblings by their canonical names,
            // which are not callable once the tools are renamed.
            let described = tool["description"]
                .as_str()
                .map(|text| rewrite_tool_mentions(text, &renames));
            if let Some(described) = described {
                tool["description"] = json!(described);
            }
        }
    }

    if !system.is_empty()
        && let Some(messages) = body["messages"].as_array_mut()
    {
        let instruction = json!({
            "type": "text",
            "text": rewrite_tool_mentions(system, &renames),
        });
        if let Some(message) = messages
            .iter_mut()
            .find(|message| message["role"].as_str() == Some("user"))
            && let Some(content) = message["content"].as_array_mut()
        {
            content.insert(0, instruction);
        } else {
            messages.insert(0, json!({"role": "user", "content": [instruction]}));
        }
    }

    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            let Some(content) = message["content"].as_array_mut() else {
                continue;
            };
            for block in content {
                // A historical call may name a tool no longer registered, so
                // this still maps lazily rather than looking the name up.
                if block["type"].as_str() == Some("tool_use")
                    && let Some(name) = block["name"].as_str()
                {
                    block["name"] = json!(mapped_oauth_tool_name(name, &mut renames));
                }
            }
        }
    }

    renames
        .into_iter()
        .map(|(canonical, wire)| (wire, canonical))
        .collect()
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_fence_byte(byte: u8) -> bool {
    byte == b'`' || byte == b'*'
}

/// Rewrites whole-word tool mentions in prose to the names the request carries.
/// Word runs are ASCII by construction, so every index lands on a char boundary.
fn rewrite_tool_mentions(text: &str, renames: &HashMap<String, String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut start = 0;
    while start < bytes.len() {
        if !is_word_byte(bytes[start]) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && is_word_byte(bytes[end]) {
            end += 1;
        }
        let word = &text[start..end];
        if let Some(wire) = renames.get(word) {
            let fenced = start > 0
                && is_fence_byte(bytes[start - 1])
                && end < bytes.len()
                && is_fence_byte(bytes[end]);
            if fenced || !AMBIGUOUS_TOOL_WORDS.contains(&word) {
                out.push_str(&text[copied..start]);
                out.push_str(wire);
                copied = end;
            }
        }
        start = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn first_user_text(body: &Value) -> Option<&str> {
    body["messages"].as_array()?.iter().find_map(|message| {
        if message["role"].as_str() != Some("user") {
            return None;
        }
        message["content"].as_array()?.iter().find_map(|block| {
            (block["type"].as_str() == Some("text"))
                .then(|| block["text"].as_str())
                .flatten()
        })
    })
}

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn billing_header(first_user_text: &str, version: &str) -> String {
    let chars: Vec<char> = first_user_text.chars().collect();
    let sampled: String = [4, 7, 20]
        .into_iter()
        .map(|index| chars.get(index).copied().unwrap_or('0'))
        .collect();
    let version_hash = hex_encode(&Sha256::digest(
        format!("{BILLING_PREFIX}{sampled}{version}").as_bytes(),
    ));
    format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=cli; cch=00000;",
        &version_hash[..3]
    )
}

fn oauth_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return "mcp_".into();
    };
    format!("mcp_{}{}", first.to_uppercase(), chars.as_str())
}

/// Registers `name` in the canonical-to-wire map, returning the wire name.
fn mapped_oauth_tool_name(name: &str, renames: &mut HashMap<String, String>) -> String {
    if let Some(wire) = renames.get(name) {
        return wire.clone();
    }
    let mut wire = oauth_tool_name(name);
    if wire.len() > MAX_TOOL_NAME || renames.values().any(|taken| taken == &wire) {
        let digest = hex_encode(&Sha256::digest(name.as_bytes()));
        let suffix = format!("_{}", &digest[..12]);
        let max_prefix = MAX_TOOL_NAME - suffix.len();
        let mut truncate_at = max_prefix.min(wire.len());
        while !wire.is_char_boundary(truncate_at) {
            truncate_at -= 1;
        }
        wire.truncate(truncate_at);
        wire.push_str(&suffix);
    }
    renames.insert(name.to_string(), wire.clone());
    wire
}

pub(super) struct EventParser {
    content_blocks: Vec<ContentBlock>,
    current_tool_json: String,
    current_tool_open: bool,
    invalid_tool_inputs: HashMap<String, InvalidToolInput>,
    message_stopped: bool,
    current_block_idx: usize,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
    oauth_tool_names: Option<HashMap<String, String>>,
}

impl EventParser {
    pub fn new() -> Self {
        Self::with_oauth_tool_names(None)
    }

    pub fn new_oauth(tool_names: HashMap<String, String>) -> Self {
        Self::with_oauth_tool_names(Some(tool_names))
    }

    fn with_oauth_tool_names(oauth_tool_names: Option<HashMap<String, String>>) -> Self {
        Self {
            content_blocks: Vec::new(),
            current_tool_json: String::new(),
            current_tool_open: false,
            invalid_tool_inputs: HashMap::new(),
            message_stopped: false,
            current_block_idx: 0,
            usage: TokenUsage::default(),
            stop_reason: None,
            oauth_tool_names,
        }
    }

    pub async fn process(
        &mut self,
        event_type: &str,
        data: &str,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<ControlFlow<(), ()>, AgentError> {
        match event_type {
            "message_start" => {
                event_tx
                    .send_async(ProviderEvent::ToolAliases {
                        aliases: self.oauth_tool_names.clone().map(Arc::new),
                    })
                    .await?;
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data) {
                    if let Some(u) = ev.message.usage {
                        self.usage = TokenUsage::from(u);
                    }
                    warn_dropped_thinking(&ev.message.input_transformations.unwrap_or_default());
                }
            }
            "content_block_start" => match serde_json::from_str::<ContentBlockStartEvent>(data) {
                Ok(ev) => {
                    self.current_block_idx = ev.index;
                    match ev.content_block {
                        SseContentBlock::Text => {
                            self.content_blocks.push(ContentBlock::Text {
                                text: String::new(),
                            });
                        }
                        SseContentBlock::Thinking => {
                            self.content_blocks
                                .push(ContentBlock::thinking(String::new(), None));
                        }
                        SseContentBlock::RedactedThinking { data } => {
                            self.content_blocks
                                .push(ContentBlock::RedactedThinking { data });
                        }
                        SseContentBlock::ToolUse { id, name, input } => {
                            let name = match &self.oauth_tool_names {
                                Some(names) => names.get(&name).cloned().unwrap_or(name),
                                None => name,
                            };
                            self.current_tool_json.clear();
                            self.current_tool_open = true;
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                    source_ordinal: None,
                                })
                                .await?;
                            self.content_blocks
                                .push(ContentBlock::tool_use(id, name, input));
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_start"),
            },
            "content_block_delta" => match serde_json::from_str::<ContentBlockDeltaEvent>(data) {
                Ok(ev) => {
                    self.current_block_idx = ev.index;
                    let block = self.content_blocks.get_mut(self.current_block_idx);
                    match ev.delta {
                        Delta::Text { text } => {
                            if !text.is_empty() {
                                if let Some(ContentBlock::Text { text: t }) = block {
                                    t.push_str(&text);
                                }
                                event_tx
                                    .send_async(ProviderEvent::TextDelta { text })
                                    .await?;
                            }
                        }
                        Delta::Thinking { thinking } => {
                            if !thinking.is_empty() {
                                if let Some(ContentBlock::Thinking { thinking: t, .. }) = block {
                                    t.push_str(&thinking);
                                }
                                event_tx
                                    .send_async(ProviderEvent::ThinkingDelta { text: thinking })
                                    .await?;
                            }
                        }
                        Delta::Signature { signature } => {
                            if let Some(ContentBlock::Thinking { signature: sig, .. }) = block {
                                *sig = Some(signature);
                            }
                        }
                        Delta::InputJson { partial_json } => {
                            append_tool_input(&mut self.current_tool_json, &partial_json);
                            if let Some(ContentBlock::ToolUse { id, .. }) = block
                                && !partial_json.is_empty()
                            {
                                event_tx
                                    .send_async(ProviderEvent::ToolInputDelta {
                                        id: id.clone(),
                                        delta: partial_json,
                                    })
                                    .await?;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_delta"),
            },
            "content_block_stop" => {
                if let Some(ContentBlock::ToolUse {
                    id, name, input, ..
                }) = self.content_blocks.get_mut(self.current_block_idx)
                {
                    if !self.current_tool_json.is_empty() || input.is_null() {
                        let (parsed, invalid) = parse_tool_input(&self.current_tool_json, false);
                        *input = parsed;
                        if let Some(invalid) = invalid {
                            self.invalid_tool_inputs.insert(id.clone(), invalid);
                        }
                    }
                    event_tx
                        .send_async(ProviderEvent::ToolInputReady {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            invalid_input: self.invalid_tool_inputs.get(id).cloned(),
                        })
                        .await?;
                    self.current_tool_json.clear();
                    self.current_tool_open = false;
                }
            }
            "message_delta" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(data) {
                    if let Some(u) = ev.usage {
                        self.usage.output = u.output_tokens;
                    }
                    if let Some(d) = ev.delta {
                        self.stop_reason = d
                            .stop_reason
                            .map(|s| StopReason::from_anthropic(&s))
                            .or(self.stop_reason.take());
                    }
                }
            }
            "error" => {
                if let Ok(ev) = serde_json::from_str::<super::super::SseErrorPayload>(data) {
                    warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error event");
                    return Err(ev.into_agent_error());
                }
                warn!(raw = %data, "unparseable SSE error event");
                return Err(AgentError::api(400, data.to_string()));
            }
            "message_stop" => {
                self.message_stopped = true;
                return Ok(ControlFlow::Break(()));
            }
            _ => {}
        }

        Ok(ControlFlow::Continue(()))
    }

    pub fn finish(mut self) -> StreamResponse {
        let complete = self.message_stopped
            && self
                .stop_reason
                .is_some_and(|reason| reason != StopReason::MaxTokens);
        for invalid in self.invalid_tool_inputs.values_mut() {
            invalid.complete = complete;
        }
        if self.current_tool_open
            && let Some(ContentBlock::ToolUse { id, input, .. }) =
                self.content_blocks.get_mut(self.current_block_idx)
        {
            let (parsed, invalid) = parse_tool_input(&self.current_tool_json, false);
            *input = parsed;
            if let Some(invalid) = invalid {
                self.invalid_tool_inputs.insert(id.clone(), invalid);
            }
        }
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content_blocks,
                ..Default::default()
            },
            usage: self.usage,
            stop_reason: self.stop_reason,
            tool_name_aliases: self.oauth_tool_names.map(Arc::new),
            invalid_tool_inputs: self.invalid_tool_inputs,
        }
    }
}

/// Levels these models declare, matching the models.dev catalog. Claude reasons
/// unconditionally unless it declares a toggle, and no Claude model has ever
/// accepted `minimal`, so the canonical fallback ladder would offer a level the
/// API rejects.
const EFFORT_TO_MAX: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "low", "medium", "high", "xhigh", "max",
])];
const TOGGLE_WITH_EFFORT_TO_MAX: &[StaticReasoningOption] = &[
    StaticReasoningOption::Toggle,
    StaticReasoningOption::Effort(&["low", "medium", "high", "xhigh", "max"]),
];
const EFFORT_TO_MAX_WITH_BUDGET: &[StaticReasoningOption] = &[
    StaticReasoningOption::Effort(&["low", "medium", "high", "max"]),
    StaticReasoningOption::BudgetTokens {
        min: Some(1_024),
        max: None,
    },
];
const EFFORT_TO_HIGH_WITH_BUDGET: &[StaticReasoningOption] = &[
    StaticReasoningOption::Effort(&["low", "medium", "high"]),
    StaticReasoningOption::BudgetTokens {
        min: Some(1_024),
        max: None,
    },
];
/// Pre-effort models: a token budget is the only knob they take.
const BUDGET_ONLY: &[StaticReasoningOption] = &[StaticReasoningOption::BudgetTokens {
    min: Some(1_024),
    max: None,
}];

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["claude-haiku-4-5"],
            small: true,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(BUDGET_ONLY),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(BUDGET_ONLY),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(64000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-5-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(TOGGLE_WITH_EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(TOGGLE_WITH_EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                // Fast mode withdrawn on 2026-06-29.
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                // Fast mode withdrawn on 2026-07-24.
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-8"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 10.00,
                    output: 50.00,
                    cache_write: 12.50,
                    cache_read: 1.00,
                }),
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-5-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 4.00,
                output: 20.00,
                cache_write: 5.00,
                cache_read: 0.20,
                fast: Some(FastPricing {
                    input: 8.00,
                    output: 40.00,
                    cache_write: 10.00,
                    cache_read: 0.40,
                }),
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 10.00,
                    output: 50.00,
                    cache_write: 12.50,
                    cache_read: 1.00,
                }),
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-fable-5-1"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 0.25,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-fable-5"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 1.00,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-0", "claude-opus-4-1"],
            small: false,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 15.00,
                output: 75.00,
                cache_write: 18.75,
                cache_read: 1.50,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(32000),
            context_window: 200_000,
            reasoning_options: None,
        },
    ];
    MODELS
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::EventParser;
    use crate::providers::test_support::{
        PEER_ATTACK, PEER_TEXT, assert_peer_framing, peer_message_origin, task_event_origin,
        task_observation_with_output_refs, workflow_event_origin,
    };
    use crate::{
        ContentBlock, Message, Model, ProviderEvent, StandingReminderKind, SteeringKind,
        ThinkingConfig, TokenUsage, invalid_tool_input,
    };

    const STEERING_TEXT: &str = "Continue with a useful response.";
    const STEERING_RULE: &str = "empty_output";
    const TOOL_ID: &str = "call_1";
    const TOOL_NAME: &str = "shell";
    const MALFORMED_COMMAND: &str = r#"{"command":"echo safe""#;
    const START_USAGE: TokenUsage = TokenUsage {
        input: 12,
        output: 1,
        cache_creation: 0,
        cache_read: 30,
    };

    #[test_case("tool_use", true, true ; "completed_response")]
    #[test_case("max_tokens", true, false ; "token_truncated_after_block_stop")]
    #[test_case("tool_use", false, false ; "transport_truncated_after_block_stop")]
    fn malformed_block_waits_for_message_stop(reason: &str, stopped: bool, complete: bool) {
        smol::block_on(async {
            let (tx, rx) = flume::unbounded();
            let mut parser = EventParser::new();
            let events = [
                (
                    "content_block_start",
                    json!({"index":0,"content_block":{"type":"tool_use","id":TOOL_ID,"name":TOOL_NAME,"input":{}}}),
                ),
                (
                    "content_block_delta",
                    json!({"index":0,"delta":{"type":"input_json_delta","partial_json":MALFORMED_COMMAND}}),
                ),
                ("content_block_stop", json!({"index":0})),
            ];
            for (event, data) in events {
                let _ = parser.process(event, &data.to_string(), &tx).await.unwrap();
            }
            let invalid = rx
                .drain()
                .find_map(|event| match event {
                    ProviderEvent::ToolInputReady { invalid_input, .. } => invalid_input,
                    _ => None,
                })
                .unwrap();
            assert_eq!(invalid.raw, MALFORMED_COMMAND);
            assert!(!invalid.complete);
            let delta = json!({"delta":{"stop_reason":reason}});
            let _ = parser
                .process("message_delta", &delta.to_string(), &tx)
                .await
                .unwrap();
            if stopped {
                let _ = parser.process("message_stop", "{}", &tx).await.unwrap();
            }
            let response = parser.finish();
            let invalid = &response.invalid_tool_inputs[TOOL_ID];
            assert_eq!(invalid.raw, MALFORMED_COMMAND);
            assert_eq!(invalid.complete, complete);
            assert!(!invalid.clipped);
            assert_eq!(
                response.message.tool_uses().next().unwrap().2,
                &invalid_tool_input(MALFORMED_COMMAND)
            );
        });
    }

    #[test_case(r#"{"INVALID_JSON":"display","caudra_invalid_json_raw":"{\"command\":\"embedded\"}","caudra_invalid_json_complete":true,"caudra_invalid_json_clipped":false,"command":"actual"}"# ; "spoofed_metadata")]
    fn valid_marker_fields_survive_wire_projection(raw: &str) {
        let input: Value = serde_json::from_str(raw).unwrap();
        let message = Message {
            content: vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, input.clone())],
            ..Message::default()
        };
        let wire = serde_json::to_value(super::build_wire_messages(&[message])).unwrap();
        assert_eq!(wire[0]["content"][0]["input"], input);
    }

    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Recovery) ; "recovery")]
    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Advisory) ; "advisory")]
    #[test_case(Message::task_observation(STEERING_TEXT.into(), task_event_origin()) ; "task_event")]
    #[test_case(task_observation_with_output_refs(STEERING_TEXT); "retained_task_outputs")]
    #[test_case(Message::workflow_observation(STEERING_TEXT.into(), workflow_event_origin()) ; "workflow_event")]
    #[test_case(Message::standing_reminder(STEERING_TEXT.into(), StandingReminderKind::BackgroundWork) ; "background_reminder")]
    fn observation_metadata_is_not_on_wire(message: Message) {
        let messages = [message];
        let wire = serde_json::to_value(super::build_wire_messages(&messages)).unwrap();
        assert_eq!(
            wire,
            json!([{"role": "user", "content": [{"type": "text", "text": STEERING_TEXT, "cache_control": {"type": "ephemeral"}}]}])
        );
    }

    #[test_case(PEER_TEXT ; "plain_text")]
    #[test_case(PEER_ATTACK ; "adversarial_host_markers")]
    fn peer_observation_wire_contains_only_framed_text(text: &str) {
        let origin = peer_message_origin();
        let messages = [Message::peer_observation(text.into(), origin.clone())];
        let wire = serde_json::to_value(super::build_wire_messages(&messages)).unwrap();
        let framed = wire[0]["content"][0]["text"].as_str().unwrap();
        assert_peer_framing(framed, text, &origin);
        assert_eq!(
            wire,
            json!([{"role": "user", "content": [{"type": "text", "text": framed, "cache_control": {"type": "ephemeral"}}]}])
        );
    }

    #[test_case("anthropic/claude-sonnet-5", &["low", "medium", "high", "xhigh", "max"] ; "sonnet_5_has_no_minimal")]
    #[test_case("anthropic/claude-sonnet-5-5", &["low", "medium", "high", "xhigh", "max"] ; "sonnet_5_5_has_no_minimal")]
    #[test_case("anthropic/claude-opus-5",   &["low", "medium", "high", "xhigh", "max"] ; "opus_5_has_no_minimal")]
    #[test_case("anthropic/claude-opus-4-5", &["low", "medium", "high"]                 ; "opus_4_5_stops_at_high")]
    #[test_case("anthropic/claude-sonnet-4-5", &["high", "max"]                         ; "budget_only_model_gets_two_steps")]
    #[test_case("openai/gpt-5.6-sol",        &["low", "medium", "high", "xhigh", "max"] ; "declared_none_is_not_a_depth")]
    fn effort_ladder_offers_only_what_the_model_declares(spec: &str, expected: &[&str]) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.reasoning_options().effort_ladder(), expected);
    }

    const REJECTED_LEVEL: &str = "a level the model never declared must never reach the wire";

    #[test_case("anthropic/claude-sonnet-5", "minimal", "low"  ; "minimal_snaps_up_to_the_declared_floor")]
    #[test_case("anthropic/claude-opus-4-5", "max",     "high" ; "max_snaps_down_to_the_declared_top")]
    fn undeclared_level_snaps_into_the_declared_ladder(spec: &str, asked: &str, expected: &str) {
        let model = Model::from_spec(spec).unwrap();
        let options = model.reasoning_options();
        assert_eq!(options.snap(asked), Some(expected), "{REJECTED_LEVEL}");
    }

    /// Off resolves through the table: Sonnet's toggle keeps it off rather than
    /// snapping it to a level, and the version decides how it is said.
    #[test_case("anthropic/claude-sonnet-5", json!({"type": "disabled"}) ; "sonnet_5_says_disabled")]
    #[test_case("anthropic/claude-sonnet-5-5", json!({"type": "between_tools"}) ; "sonnet_5_5_says_between_tools")]
    fn thinking_off_reaches_the_wire_as_the_model_spells_it(spec: &str, expected: Value) {
        let model = Model::from_spec(spec).unwrap();
        let body = super::request_body(&model, &[], None, "", &json!([]), &ThinkingConfig::Off);
        assert_eq!(body["thinking"], expected);
    }

    /// The drop report shares `message_start` with the usage, so no shape of
    /// it may cost the turn its token count.
    #[test_case(json!([{"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "prefix_binding_mismatch"}]) ; "a_dropped_block")]
    #[test_case(json!([{"type": "a_later_kind"}]) ; "an_unrecognized_entry")]
    #[test_case(json!(null) ; "an_explicit_null")]
    fn input_transformations_keep_the_usage(transformations: Value) {
        smol::block_on(async {
            let (tx, _rx) = flume::unbounded();
            let mut parser = EventParser::new();
            let start = json!({"type": "message_start", "message": {
                "usage": START_USAGE,
                "input_transformations": transformations,
            }});
            let _ = parser
                .process("message_start", &start.to_string(), &tx)
                .await
                .unwrap();
            assert_eq!(parser.finish().usage, START_USAGE);
        });
    }

    use super::{
        AMBIGUOUS_TOOL_WORDS, LONG_CONTEXT_SUFFIX, LONG_CONTEXT_WINDOW, WIDE_CONTEXT_WINDOW,
        long_context_window, mapped_oauth_tool_name, rewrite_tool_mentions, strip_long_context,
    };
    use std::collections::HashMap;

    #[test_case("claude-opus-4-8-1m", "claude-opus-4-8" ; "strips_suffix")]
    #[test_case("claude-opus-4-8", "claude-opus-4-8" ; "leaves_plain_id")]
    fn strip_long_context_removes_suffix(model_id: &str, expected: &str) {
        assert_eq!(strip_long_context(model_id), expected);
    }

    #[test_case("claude-opus-4-8-1m", Some(LONG_CONTEXT_WINDOW) ; "suffix_opts_in")]
    #[test_case("claude-opus-4-8", None ; "plain_id_keeps_base")]
    fn long_context_window_follows_suffix(model_id: &str, expected: Option<u32>) {
        assert_eq!(long_context_window(model_id), expected);
        assert!(LONG_CONTEXT_SUFFIX.ends_with("1m"));
    }

    const NARROW_CONTEXT_WINDOW: u32 = 200_000;

    /// Only the window caudra picks itself is an input budget. The 200k entries
    /// and the 1M ceiling are API totals, so they keep the larger reserve.
    #[test_case("anthropic/claude-sonnet-4-6", WIDE_CONTEXT_WINDOW,   true  ; "sonnet_4_6_is_wide")]
    #[test_case("anthropic/claude-sonnet-5", WIDE_CONTEXT_WINDOW,     true  ; "sonnet_5_is_wide")]
    #[test_case("anthropic/claude-sonnet-5-5", WIDE_CONTEXT_WINDOW,   true  ; "sonnet_5_5_is_wide")]
    #[test_case("anthropic/claude-opus-4-8", WIDE_CONTEXT_WINDOW,     true  ; "opus_4_8_is_wide")]
    #[test_case("anthropic/claude-opus-5", WIDE_CONTEXT_WINDOW,       true  ; "opus_5_is_wide")]
    #[test_case("anthropic/claude-fable-5", WIDE_CONTEXT_WINDOW,      true  ; "fable_5_is_wide")]
    #[test_case("anthropic/claude-opus-4-5", NARROW_CONTEXT_WINDOW,   false ; "opus_4_5_stays_narrow")]
    #[test_case("anthropic/claude-sonnet-4-5", NARROW_CONTEXT_WINDOW, false ; "sonnet_4_5_stays_narrow")]
    #[test_case("anthropic/claude-haiku-4-5", NARROW_CONTEXT_WINDOW,  false ; "haiku_4_5_stays_narrow")]
    #[test_case("anthropic/claude-opus-5-1m", LONG_CONTEXT_WINDOW,    false ; "suffix_still_opts_into_the_ceiling")]
    // Only this table declares input budgets. An OpenAI entry at the same number
    // is an API total, and a rule that read the resolved window rather than its
    // source would have reserved the wrong share of it.
    #[test_case("openai/gpt-5.6-sol", WIDE_CONTEXT_WINDOW,            false ; "another_table_at_the_same_number_is_a_total")]
    fn context_window_matches_the_declared_tier(spec: &str, expected: u32, excludes_output: bool) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.context_window, expected);
        assert_eq!(model.window_excludes_output, excludes_output);
    }

    #[test_case("bash" ; "builtin")]
    #[test_case("mcp_fetch" ; "already_prefixed")]
    fn oauth_tool_names_round_trip(name: &str) {
        let mut renames = HashMap::new();
        let wire = mapped_oauth_tool_name(name, &mut renames);
        assert_eq!(renames[name], wire);
    }

    /// The prose rules, pinned one case at a time. An unambiguous name is
    /// rewritten anywhere; a name that is also an English word only when fenced.
    #[test_case("use file_read now", "use mcp_File_read now" ; "bare_compound_name")]
    #[test_case("`file_read`", "`mcp_File_read`" ; "fenced_compound_name")]
    #[test_case("see code_map.", "see mcp_Code_map." ; "trailing_punctuation")]
    #[test_case("file_reader", "file_reader" ; "longer_word_is_not_a_mention")]
    #[test_case("my_file_read", "my_file_read" ; "suffix_is_not_a_mention")]
    #[test_case("a shell command pipeline", "a shell command pipeline" ; "english_word_survives")]
    #[test_case("shell children inherit", "shell children inherit" ; "english_word_at_start_survives")]
    #[test_case("`shell`", "`mcp_Shell`" ; "fenced_english_word_is_a_mention")]
    #[test_case("**shell**", "**mcp_Shell**" ; "bold_english_word_is_a_mention")]
    #[test_case("nothing to do", "nothing to do" ; "untouched_prose")]
    #[test_case("", "" ; "empty")]
    #[test_case("café file_read", "café mcp_File_read" ; "multibyte_neighbour")]
    fn tool_mentions_rewrite_by_fencing(text: &str, expected: &str) {
        let renames = HashMap::from([
            ("file_read".to_string(), "mcp_File_read".to_string()),
            ("code_map".to_string(), "mcp_Code_map".to_string()),
            ("shell".to_string(), "mcp_Shell".to_string()),
        ]);
        assert_eq!(rewrite_tool_mentions(text, &renames), expected);
    }

    /// The stoplist is scanned linearly per matched word, so keep it sorted and
    /// free of duplicates for review.
    #[test]
    fn ambiguous_words_are_sorted_and_unique() {
        let mut sorted = AMBIGUOUS_TOOL_WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, AMBIGUOUS_TOOL_WORDS);
    }

    use serde_json::{Value, json};

    use super::{EAGER_INPUT_STREAMING, apply_oauth_request_profile, build_wire_tools};

    const BUFFERED_TOOL: &str =
        "every tool must ask for eager streaming or its large arguments arrive in one fragment";
    const BREAKPOINT_MOVED: &str = "only the last tool carries the cache breakpoint";

    fn wire_tools() -> Vec<Value> {
        let tools = json!([
            {"name": "file_read", "input_schema": {}},
            {"name": "file_write", "input_schema": {}},
        ]);
        build_wire_tools(&tools).as_array().unwrap().clone()
    }

    #[test]
    fn every_wire_tool_asks_for_eager_input_streaming() {
        let tools = wire_tools();
        assert!(
            tools
                .iter()
                .all(|t| t[EAGER_INPUT_STREAMING] == json!(true)),
            "{BUFFERED_TOOL}"
        );
        assert!(tools[0]["cache_control"].is_null(), "{BREAKPOINT_MOVED}");
        assert!(!tools[1]["cache_control"].is_null(), "{BREAKPOINT_MOVED}");
    }

    /// The OAuth profile rewrites tool names in place, so the field it does not
    /// know about has to survive the rename it does perform.
    #[test]
    fn the_oauth_rename_keeps_eager_input_streaming() {
        let mut body = json!({
            "system": [],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": wire_tools(),
        });
        apply_oauth_request_profile(&mut body, "", "1.0.0");

        let tools = body["tools"].as_array().unwrap();
        assert!(
            tools
                .iter()
                .all(|t| t[EAGER_INPUT_STREAMING] == json!(true)),
            "{BUFFERED_TOOL}"
        );
        assert!(tools[1]["name"].as_str().unwrap().starts_with("mcp_"));
    }
}
