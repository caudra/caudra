use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Arc, LazyLock};

use flume::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::model::{
    FastPricing, Model, ModelEntry, ModelFamily, ModelPricing, ModelTier, StaticReasoningOption,
};
use crate::{
    AgentError, ContentBlock, EMPTY_RESPONSE_MARKER, Message, ProviderEvent, Role, StopReason,
    StreamResponse, ThinkingConfig, TokenUsage,
};

pub(super) const BETA_TOOL_EXAMPLES_BEDROCK: &str = "tool-examples-2025-10-29";

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
/// cost and latency, the same way `GPT_5_6_CONTEXT_WINDOW` caps OpenAI's.
pub(crate) const WIDE_CONTEXT_WINDOW: u32 = 376_000;

const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &str = "59cf53e54c78";

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

pub(super) const MESSAGE_CACHE_BREAKPOINTS: usize = 2;

static EMPTY_CONTENT: LazyLock<ContentBlock> = LazyLock::new(|| ContentBlock::Text {
    text: EMPTY_RESPONSE_MARKER.into(),
});

#[derive(Serialize)]
pub(crate) struct CacheControl {
    pub r#type: &'static str,
}

pub(crate) const EPHEMERAL: CacheControl = CacheControl {
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

#[derive(Deserialize)]
struct MessagePayload {
    #[serde(default)]
    usage: Option<Usage>,
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
    RedactedThinking { data: String },
    ToolUse { id: String, name: String },
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
pub(crate) struct SystemBlock<'a> {
    pub r#type: &'static str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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

pub(super) fn build_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

pub(crate) fn build_request_body_with_system(
    model: &Model,
    messages: &[Message],
    system_blocks: &[SystemBlock<'_>],
    tools: &Value,
    thinking: ThinkingConfig,
) -> Value {
    let wire_messages = build_wire_messages(messages);
    let wire_tools = build_wire_tools(tools);

    let mut body = json!({
        "max_tokens": model.max_output_tokens.unwrap_or(FALLBACK_MAX_TOKENS),
        "system": system_blocks,
        "messages": wire_messages,
        "tools": wire_tools,
    });

    thinking.apply_to_body(&mut body, model);
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

    if !system.is_empty()
        && let Some(messages) = body["messages"].as_array_mut()
    {
        let instruction = json!({"type": "text", "text": system});
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

    let mut tool_names = HashMap::new();
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                tool["name"] = json!(mapped_oauth_tool_name(name, &mut tool_names));
            }
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            let Some(content) = message["content"].as_array_mut() else {
                continue;
            };
            for block in content {
                if block["type"].as_str() == Some("tool_use")
                    && let Some(name) = block["name"].as_str()
                {
                    block["name"] = json!(mapped_oauth_tool_name(name, &mut tool_names));
                }
            }
        }
    }
    tool_names
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

fn billing_header(first_user_text: &str, version: &str) -> String {
    let chars: Vec<char> = first_user_text.chars().collect();
    let sampled: String = [4, 7, 20]
        .into_iter()
        .map(|index| chars.get(index).copied().unwrap_or('0'))
        .collect();
    let version_hash = format!(
        "{:x}",
        Sha256::digest(format!("{BILLING_PREFIX}{sampled}{version}").as_bytes())
    );
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

fn mapped_oauth_tool_name(name: &str, names: &mut HashMap<String, String>) -> String {
    if let Some((wire, _)) = names.iter().find(|(_, original)| original.as_str() == name) {
        return wire.clone();
    }
    let mut wire = oauth_tool_name(name);
    if wire.len() > 64 || names.contains_key(&wire) {
        let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
        let suffix = format!("_{}", &digest[..12]);
        let max_prefix = 64 - suffix.len();
        let mut truncate_at = max_prefix.min(wire.len());
        while !wire.is_char_boundary(truncate_at) {
            truncate_at -= 1;
        }
        wire.truncate(truncate_at);
        wire.push_str(&suffix);
    }
    names.insert(wire.clone(), name.to_string());
    wire
}

fn canonical_tool_name(name: &str) -> String {
    let Some(name) = name.strip_prefix("mcp_") else {
        return name.to_string();
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    format!("{}{}", first.to_lowercase(), chars.as_str())
}

pub(super) struct EventParser {
    content_blocks: Vec<ContentBlock>,
    current_tool_json: String,
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
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data)
                    && let Some(u) = ev.message.usage
                {
                    self.usage = TokenUsage::from(u);
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
                        SseContentBlock::ToolUse { id, name } => {
                            let name = match &self.oauth_tool_names {
                                Some(names) => names
                                    .get(&name)
                                    .cloned()
                                    .unwrap_or_else(|| canonical_tool_name(&name)),
                                None => name,
                            };
                            self.current_tool_json.clear();
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await?;
                            self.content_blocks
                                .push(ContentBlock::tool_use(id, name, Value::Null));
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
                            self.current_tool_json.push_str(&partial_json);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_delta"),
            },
            "content_block_stop" => {
                if let Some(ContentBlock::ToolUse { name, input, .. }) =
                    self.content_blocks.get_mut(self.current_block_idx)
                {
                    *input = match serde_json::from_str(&self.current_tool_json) {
                        Ok(v) => {
                            debug!(tool = %name, json = %self.current_tool_json, "tool input JSON");
                            v
                        }
                        Err(e) => {
                            warn!(error = %e, json = %self.current_tool_json, "malformed tool JSON, falling back to {{}}");
                            Value::Object(Default::default())
                        }
                    };
                    self.current_tool_json.clear();
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
                return Err(AgentError::Api {
                    status: 400,
                    message: data.to_string(),
                });
            }
            "message_stop" => return Ok(ControlFlow::Break(())),
            _ => {}
        }

        Ok(ControlFlow::Continue(()))
    }

    pub fn finish(self) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content_blocks,
                ..Default::default()
            },
            usage: self.usage,
            stop_reason: self.stop_reason,
            tool_name_aliases: self.oauth_tool_names.map(Arc::new),
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
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(BUDGET_ONLY),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(BUDGET_ONLY),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            // Introductory rates until 2026-09-01, then 3.00 / 15.00 / 3.75 / 0.30.
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(TOGGLE_WITH_EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6"],
            tier: ModelTier::Strong,
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
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX_WITH_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7"],
            tier: ModelTier::Strong,
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
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-8"],
            tier: ModelTier::Strong,
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
                }),
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 10.00,
                    output: 50.00,
                }),
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-fable-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 1.00,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128000),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-4-0", "claude-opus-4-1"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 15.00,
                output: 75.00,
                cache_write: 18.75,
                cache_read: 1.50,
                fast: None,
                tiers: Vec::new(),
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

    use crate::Model;

    #[test_case("anthropic/claude-sonnet-5", &["low", "medium", "high", "xhigh", "max"] ; "sonnet_5_has_no_minimal")]
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

    use super::{
        LONG_CONTEXT_SUFFIX, LONG_CONTEXT_WINDOW, WIDE_CONTEXT_WINDOW, canonical_tool_name,
        long_context_window, oauth_tool_name, strip_long_context,
    };

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

    #[test_case("anthropic/claude-sonnet-4-6", WIDE_CONTEXT_WINDOW   ; "sonnet_4_6_is_wide")]
    #[test_case("anthropic/claude-sonnet-5", WIDE_CONTEXT_WINDOW     ; "sonnet_5_is_wide")]
    #[test_case("anthropic/claude-opus-4-8", WIDE_CONTEXT_WINDOW     ; "opus_4_8_is_wide")]
    #[test_case("anthropic/claude-opus-5", WIDE_CONTEXT_WINDOW       ; "opus_5_is_wide")]
    #[test_case("anthropic/claude-fable-5", WIDE_CONTEXT_WINDOW      ; "fable_5_is_wide")]
    #[test_case("anthropic/claude-opus-4-5", NARROW_CONTEXT_WINDOW   ; "opus_4_5_stays_narrow")]
    #[test_case("anthropic/claude-sonnet-4-5", NARROW_CONTEXT_WINDOW ; "sonnet_4_5_stays_narrow")]
    #[test_case("anthropic/claude-haiku-4-5", NARROW_CONTEXT_WINDOW  ; "haiku_4_5_stays_narrow")]
    #[test_case("anthropic/claude-opus-5-1m", LONG_CONTEXT_WINDOW    ; "suffix_still_opts_into_the_ceiling")]
    fn context_window_matches_the_declared_tier(spec: &str, expected: u32) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.context_window, expected);
    }

    #[test_case("bash" ; "builtin")]
    #[test_case("mcp_fetch" ; "already_prefixed")]
    fn oauth_tool_names_round_trip(name: &str) {
        assert_eq!(canonical_tool_name(&oauth_tool_name(name)), name);
    }
}
