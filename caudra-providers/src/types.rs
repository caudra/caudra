//! Message and content types for provider communication.
//! `Message.display_text`: `Some("")` marks a message as synthetic (sent to the API but hidden
//! from the UI). `user_text()` returns `None` for these, so system-injected messages
//! (cancel markers, compaction prompts) stay invisible without a separate type.
//! `Message.kind` answers a different question. Synthetic text is ours and
//! trusted, it is just not worth showing. An observation comes from outside,
//! belongs in model context, and must never be mistaken for the user talking.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use caudra_storage::sessions::TitleSource;
pub use caudra_storage::thinking::{
    EFFORT_LEVELS, MIN_THINKING_BUDGET, ReasoningOption, ReasoningOptions,
};
use caudra_storage::thinking::{EFFORT_NONE, StoredThinking, effort_rank};
use caudra_storage::tool_outputs::ToolOutputRef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use strum::{Display, IntoStaticStr};
use tracing::warn;

use crate::TokenUsage;
use crate::model::Model;

const LOCAL_BUDGET_FIELD: &str = "thinking_budget_tokens";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageMediaType {
    pub const ALL: [Self; 4] = [Self::Png, Self::Jpeg, Self::Gif, Self::Webp];

    /// Single source of truth for media-type strings: serde, data URLs,
    /// wire formats, and the Lua bridge all go through here.
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.mime() == mime)
    }
}

impl Serialize for ImageMediaType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.mime())
    }
}

impl<'de> Deserialize<'de> for ImageMediaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_mime(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown image media type '{s}'")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ImageSource {
    pub media_type: ImageMediaType,
    pub data: Arc<str>,
}

impl Serialize for ImageSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ImageSource", 3)?;
        state.serialize_field("type", "base64")?;
        state.serialize_field("media_type", &self.media_type)?;
        state.serialize_field("data", &self.data)?;
        state.end()
    }
}

impl ImageSource {
    pub fn new(media_type: ImageMediaType, data: Arc<str>) -> Self {
        Self { media_type, data }
    }

    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type.mime(), self.data)
    }
}

pub const IMAGE_OMITTED_NOTE: &str =
    "[image omitted: the current model does not support image input]";
/// See [`Message::empty_marker`].
pub const EMPTY_RESPONSE_MARKER: &str = "(empty)";

/// For models without vision, image blocks become a text note instead of a
/// wire block the API would reject. History keeps the pixels, so switching
/// back to a vision-capable model restores them.
pub fn adapt_images_for_model<'a>(model: &Model, messages: &'a [Message]) -> Cow<'a, [Message]> {
    let has_image = |m: &Message| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    };
    if model.supports_vision() || !messages.iter().any(has_image) {
        return Cow::Borrowed(messages);
    }
    let adapted = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            for block in &mut m.content {
                if matches!(block, ContentBlock::Image { .. }) {
                    *block = ContentBlock::Text {
                        text: IMAGE_OMITTED_NOTE.into(),
                    };
                }
            }
            m
        })
        .collect();
    Cow::Owned(adapted)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
}

impl Role {
    fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(skip)]
        output_ref: Option<ToolOutputRef>,
    },
    Image {
        source: ImageSource,
    },
}

impl ContentBlock {
    pub fn is_thinking(&self) -> bool {
        matches!(self, Self::Thinking { .. } | Self::RedactedThinking { .. })
    }
}

/// Who a message came from, which `role` cannot say. Providers only
/// accept user and assistant, so anything the host wants to report has to
/// travel as a user message, and without this there is no way to tell it
/// apart from the user actually typing. A prefix in the text would not do:
/// a log line can print one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Someone said this, the user or the model.
    #[default]
    Turn,
    /// The host noticed it and passed it to the model. It stays in session
    /// history for conversation order but is hidden from user-facing views.
    Observation,
}

impl MessageKind {
    fn is_turn(&self) -> bool {
        matches!(self, Self::Turn)
    }
}

impl ContentBlock {
    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
            thought_signature: None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
    /// Skipped when it is `Turn`, so sessions written before this existed
    /// load unchanged.
    #[serde(default, skip_serializing_if = "MessageKind::is_turn")]
    pub kind: MessageKind,
    /// One call ID per trailing tool-result image, in image order. This is
    /// runtime projection metadata and must never reach provider wires or
    /// legacy persisted messages.
    #[serde(skip)]
    pub tool_result_image_owners: Vec<String>,
    /// Session-owned artifacts retained by a compacted summary. Host-only:
    /// provider payloads must see retrieval IDs only when summary text cites them.
    #[serde(skip)]
    pub retained_output_refs: Vec<ToolOutputRef>,
    #[serde(skip)]
    pub retained_subagent_ids: Vec<String>,
    /// Marks the host-generated summary that replaced prior conversation state.
    #[serde(skip)]
    pub is_compaction_summary: bool,
}

impl Message {
    /// Stands in for an assistant turn with no text, thinking alone or empty,
    /// which providers reject as the trailing message. Never a real response:
    /// readers mining history for model text must skip it.
    pub fn empty_marker() -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: EMPTY_RESPONSE_MARKER.into(),
            }],
            ..Default::default()
        }
    }

    /// Something the host saw, reported to the model without pretending
    /// the user said it.
    pub fn observation(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            kind: MessageKind::Observation,
            ..Default::default()
        }
    }

    pub fn is_observation(&self) -> bool {
        self.kind == MessageKind::Observation
    }

    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            ..Default::default()
        }
    }

    pub fn user_display(ai_text: String, display: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: ai_text }],
            display_text: Some(display),
            ..Default::default()
        }
    }

    pub fn user_display_with_images(
        ai_text: String,
        display: String,
        images: Vec<ImageSource>,
    ) -> Self {
        let mut message = Self::user_with_images(ai_text, images);
        message.display_text = Some(display);
        message
    }

    pub fn user_with_images(text: String, images: Vec<ImageSource>) -> Self {
        let mut content: Vec<ContentBlock> = images
            .into_iter()
            .map(|source| ContentBlock::Image { source })
            .collect();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        Self {
            role: Role::User,
            content,
            ..Default::default()
        }
    }

    pub fn synthetic(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    pub fn user_text(&self) -> Option<&str> {
        match &self.display_text {
            Some(t) if t.is_empty() => None,
            Some(t) => Some(t),
            None => self.first_text_content(),
        }
    }

    pub fn first_text_content(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}

impl TitleSource for Message {
    fn first_user_text(&self) -> Option<&str> {
        if !self.role.is_user() || self.is_observation() {
            return None;
        }
        self.user_text()
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum ProviderEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Display, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl StopReason {
    pub fn from_anthropic(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_openai(s: &str) -> Self {
        match s {
            "stop" => Self::EndTurn,
            "tool_calls" => Self::ToolUse,
            "length" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_google(s: &str) -> Self {
        match s {
            "STOP" => Self::EndTurn,
            "MAX_TOKENS" => Self::MaxTokens,
            "SAFETY" | "RECITATION" => {
                warn!("Gemini stop reason: {s}, treating as end_turn");
                Self::EndTurn
            }
            _ => Self::EndTurn,
        }
    }
}

pub const THINKING_USAGE: &str = "Usage: /thinking [off|adaptive|<effort level>|<token budget>]";

/// First Claude version that speaks adaptive thinking. Opus got there a
/// generation early, at 4.7; the other families joined at 5.
const ADAPTIVE_SINCE: (u32, u32) = (5, 0);
const ADAPTIVE_SINCE_OPUS: (u32, u32) = (4, 7);
const OPUS: &str = "opus";

/// `claude-opus-4.7` -> `("opus", (4, 7))`, `claude-opus-5-1m` -> `("opus", (5, 0))`.
/// Copilot writes the version with a dot, hence the two separators. Legacy ids
/// put the version first (`claude-3-5-sonnet-20241022`), so a numeric family
/// tells us there is no modern version to read here. Gateway ids keep a
/// vendor prefix (`anthropic/claude-opus-4-7`), so read the last path segment.
fn claude_version(model_id: &str) -> Option<(&str, (u32, u32))> {
    let bare = model_id.rsplit('/').next().unwrap_or(model_id);
    let mut parts = bare.strip_prefix("claude-")?.split(['-', '.']);
    let family = parts.next().filter(|f| f.parse::<u32>().is_err())?;
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((family, (major, minor)))
}

/// How a local model spells thinking on the wire, in place of a token budget.
/// Each mode carries the JSON fragment merged into the request body, so any
/// shape a chat template needs works without a schema per provider.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThinkingFields {
    #[serde(default)]
    off: Option<Map<String, Value>>,
    #[serde(default)]
    adaptive: Option<Map<String, Value>>,
    /// Keyed by effort level. The declared keys are the levels the model
    /// accepts, so they double as its [`ReasoningOptions`].
    #[serde(flatten, deserialize_with = "deserialize_levels")]
    levels: BTreeMap<String, Map<String, Value>>,
}

/// A key caudra cannot rank is a key it cannot snap to, so it would be silently
/// unreachable. Rejecting it turns a config typo into an error the user sees.
fn deserialize_levels<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Map<String, Value>>, D::Error> {
    let levels = BTreeMap::<String, Map<String, Value>>::deserialize(deserializer)?;
    if let Some(unknown) = levels.keys().find(|key| effort_rank(key).is_none()) {
        return Err(serde::de::Error::custom(format!(
            "unknown thinking level {unknown}, expected one of: {}",
            EFFORT_LEVELS.join(", ")
        )));
    }
    Ok(levels)
}

impl ThinkingFields {
    /// The levels this model accepts, in ascending order. Reported as
    /// [`ReasoningOptions`] so a local model resolves a setting through the
    /// same path as every hosted one.
    pub fn reasoning_options(&self) -> ReasoningOptions {
        let mut values: Vec<&String> = self.levels.keys().collect();
        values.sort_by_key(|level| effort_rank(level).unwrap_or(usize::MAX));
        // A local model always has the budget field as an escape hatch, so it
        // can be switched on and off whether or not it named a fragment for it.
        let mut options = vec![ReasoningOption::Toggle];
        if !values.is_empty() {
            options.push(ReasoningOption::Effort {
                values: values.into_iter().cloned().collect(),
            });
        }
        ReasoningOptions::new(options)
    }

    /// The fragment for a resolved setting, falling back to `adaptive` when the
    /// model has no spelling for the exact mode asked for. The flag tells the
    /// caller to still send a token budget alongside it.
    fn fragment(&self, resolved: &ResolvedThinking) -> Option<(&Map<String, Value>, bool)> {
        match resolved {
            ResolvedThinking::Off => self.off.as_ref().map(|fields| (fields, false)),
            ResolvedThinking::On => self.adaptive.as_ref().map(|fields| (fields, false)),
            ResolvedThinking::Effort(level) => self
                .levels
                .get(level.as_str())
                .or(self.adaptive.as_ref())
                .map(|fields| (fields, false)),
            ResolvedThinking::Budget(_) => self.adaptive.as_ref().map(|fields| (fields, true)),
        }
    }
}

fn merge_body(body: &mut Map<String, Value>, fragment: &Map<String, Value>) {
    for (key, value) in fragment {
        match (body.get_mut(key), value.as_object()) {
            (Some(Value::Object(target)), Some(source)) => merge_body(target, source),
            _ => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

/// What a thinking setting means for one model, once resolved against the
/// levels and bounds that model declares. Providers render one of these into
/// their own wire shape; nothing else interprets a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedThinking {
    /// Reasoning off, and the model can say so.
    Off,
    /// Reasoning on with no depth named: the model picks.
    On,
    /// A level the model declared, or the level the user typed when the model
    /// declared none.
    Effort(String),
    /// A token count inside the model's declared bounds.
    Budget(u32),
}

impl ResolvedThinking {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl std::fmt::Display for ResolvedThinking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::On => f.write_str("adaptive"),
            Self::Effort(level) => f.write_str(level),
            Self::Budget(tokens) => write!(f, "{tokens}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThinkingCompatibilityError {
    #[error("model does not support thinking")]
    Unsupported,
    #[error("model does not declare whether it accepts this thinking setting")]
    UnknownOptions,
    #[error("model does not support adaptive thinking")]
    AdaptiveUnsupported,
    #[error("model would resolve thinking as {0}")]
    Inexact(ResolvedThinking),
}

/// The thinking setting exactly as the user asked for it. It stays unresolved
/// so switching models never silently rewrites the request; see
/// [`ThinkingConfig::resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ThinkingConfig {
    #[default]
    Off,
    Adaptive,
    Effort(Box<str>),
    Budget(u32),
}

impl ThinkingConfig {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The one place a setting meets a model. Every level is snapped and every
    /// budget clamped here and nowhere else, against what the model declares
    /// rather than a table caudra maintains per provider.
    pub fn resolve(&self, model: &Model) -> ResolvedThinking {
        let options = model.reasoning_options();
        let max_output = model.max_output_tokens;
        match self {
            // The model cannot be asked to stop, so the shallowest depth it
            // offers is the closest thing to what was asked for.
            Self::Off if model.requires_thinking() => match options.effort_ladder().first() {
                Some(lowest) => Self::Effort((*lowest).into()).resolve(model),
                None => ResolvedThinking::On,
            },
            // A declared `none` is how an effort model spells off.
            Self::Off if options.efforts().iter().any(|level| level == EFFORT_NONE) => {
                ResolvedThinking::Effort(EFFORT_NONE.to_string())
            }
            Self::Off => ResolvedThinking::Off,
            Self::Adaptive => ResolvedThinking::On,
            Self::Effort(level) => match options.snap(level) {
                Some(declared) => ResolvedThinking::Effort(declared.to_string()),
                None if options.budget_bounds().is_some() => {
                    ResolvedThinking::Budget(options.budget_for_effort(level, max_output))
                }
                // Nothing declared: the model never told us what it takes, so
                // the level goes through as typed.
                None => ResolvedThinking::Effort(level.to_string()),
            },
            Self::Budget(tokens) => {
                if options.efforts().is_empty() || options.budget_bounds().is_some() {
                    ResolvedThinking::Budget(options.clamp_budget(*tokens, max_output))
                } else {
                    // A level-only model has no honest token count to send, so
                    // reasoning stays on at the model's own depth.
                    warn!(
                        model = %model.id,
                        "model takes reasoning levels, not token budgets; using its default depth"
                    );
                    ResolvedThinking::On
                }
            }
        }
    }

    /// Resolves only when the model can honor the requested semantics without
    /// clamping, snapping, promotion, or translation.
    pub fn resolve_exact(
        &self,
        model: &Model,
    ) -> Result<ResolvedThinking, ThinkingCompatibilityError> {
        let requested = match self {
            Self::Effort(level) => Self::Effort(level.trim().to_ascii_lowercase().into()),
            _ => self.clone(),
        };
        if requested.is_enabled() && !model.supports_thinking() {
            return Err(ThinkingCompatibilityError::Unsupported);
        }

        let options = model.reasoning_options();
        let resolved = requested.resolve(model);
        let exact = match (&requested, &resolved) {
            (Self::Off, ResolvedThinking::Off) => true,
            (Self::Off, ResolvedThinking::Effort(level)) => level == EFFORT_NONE,
            (Self::Adaptive, ResolvedThinking::On) => true,
            (Self::Effort(requested), ResolvedThinking::Effort(resolved)) => {
                requested.as_ref() == resolved
            }
            (Self::Budget(requested), ResolvedThinking::Budget(resolved)) => requested == resolved,
            _ => false,
        };
        if !exact {
            return Err(ThinkingCompatibilityError::Inexact(resolved));
        }
        match &requested {
            Self::Adaptive
                if model
                    .id
                    .rsplit('/')
                    .next()
                    .is_some_and(|id| id.starts_with("claude-"))
                    && !Self::requires_adaptive(&model.id) =>
            {
                Err(ThinkingCompatibilityError::AdaptiveUnsupported)
            }
            Self::Effort(level)
                if !options
                    .efforts()
                    .iter()
                    .any(|value| value == level.as_ref()) =>
            {
                Err(ThinkingCompatibilityError::UnknownOptions)
            }
            Self::Budget(_) if options.budget_bounds().is_none() => {
                Err(ThinkingCompatibilityError::UnknownOptions)
            }
            _ => Ok(resolved),
        }
    }

    /// Anthropic messages API body. Adaptive-thinking models get the native
    /// adaptive knob plus `output_config.effort`; older models get a token
    /// budget, and the ones that take both get both.
    pub fn apply_to_body(&self, body: &mut Value, model: &Model) {
        let resolved = self.resolve(model);
        if Self::requires_adaptive(&model.id) {
            if !resolved.is_enabled() {
                return;
            }
            // These models default `display` to "omitted", so thinking arrives
            // empty and tool calls pop up out of nowhere in the UI. Asking for
            // the summary back costs nothing: thinking tokens bill the same.
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            if let ResolvedThinking::Effort(level) = resolved {
                body["output_config"]["effort"] = json!(level);
            }
            return;
        }
        match resolved {
            ResolvedThinking::Off => {}
            ResolvedThinking::On => body["thinking"] = json!({"type": "adaptive"}),
            ResolvedThinking::Budget(tokens) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": tokens});
            }
            // Opus 4.5 names a level but still requires the budget field.
            ResolvedThinking::Effort(level) => {
                let tokens = model
                    .reasoning_options()
                    .budget_for_effort(&level, model.max_output_tokens);
                body["thinking"] = json!({"type": "enabled", "budget_tokens": tokens});
                body["output_config"]["effort"] = json!(level);
            }
        }
    }

    /// Models from [`ADAPTIVE_SINCE`] on reject `type: "enabled"` with a 400. A
    /// version check, not an allowlist, so future releases and new families
    /// work automatically.
    fn requires_adaptive(model_id: &str) -> bool {
        claude_version(model_id).is_some_and(|(family, version)| {
            version
                >= if family == OPUS {
                    ADAPTIVE_SINCE_OPUS
                } else {
                    ADAPTIVE_SINCE
                }
        })
    }

    /// The level to send, or `None` when this model has none to name and its
    /// own default should stand.
    pub fn effort_str(&self, model: &Model) -> Option<String> {
        match self.resolve(model) {
            ResolvedThinking::Effort(level) => Some(level),
            _ => None,
        }
    }

    /// OpenAI-compatible `reasoning_effort`.
    pub fn apply_reasoning_effort(&self, body: &mut Value, model: &Model) {
        if let Some(level) = self.effort_str(model) {
            body["reasoning_effort"] = json!(level);
        }
    }

    /// Google `thinkingConfig`. Gemini 3 names a level, Gemini 2.5 takes a
    /// budget, and both want the thought summaries back.
    pub fn apply_google_thinking(&self, body: &mut Value, model: &Model) {
        let config = match self.resolve(model) {
            ResolvedThinking::Off => return,
            ResolvedThinking::On => json!({"includeThoughts": true}),
            ResolvedThinking::Effort(level) => {
                json!({"includeThoughts": true, "thinkingLevel": level})
            }
            ResolvedThinking::Budget(tokens) => {
                json!({"includeThoughts": true, "thinkingBudget": tokens})
            }
        };
        body["generationConfig"]["thinkingConfig"] = config;
    }

    pub fn apply_local_thinking(&self, body: &mut Value, model: &Model) {
        let resolved = self.resolve(model);
        if let Some(fields) = &model.thinking_fields
            && let Some((fragment, keep_budget)) = fields.fragment(&resolved)
            && let Some(object) = body.as_object_mut()
        {
            merge_body(object, fragment);
            if keep_budget && let ResolvedThinking::Budget(tokens) = resolved {
                body[LOCAL_BUDGET_FIELD] = json!(tokens);
            }
            return;
        }
        // No fragment means the model has no way to spell this mode, so the
        // budget field takes over: a request must never end up saying nothing.
        let budget = match resolved {
            ResolvedThinking::Off => 0,
            ResolvedThinking::On | ResolvedThinking::Effort(_) => -1,
            ResolvedThinking::Budget(tokens) => i64::from(tokens),
        };
        body[LOCAL_BUDGET_FIELD] = json!(budget);
    }

    pub fn parse(input: &str, current: &Self) -> Result<Self, &'static str> {
        if input.is_empty() {
            return Ok(if current.is_enabled() {
                Self::Off
            } else {
                Self::Adaptive
            });
        }
        StoredThinking::parse_setting(input)
            .map(Into::into)
            .map_err(|_| THINKING_USAGE)
    }

    pub fn status_label(&self) -> Option<Cow<'static, str>> {
        match self {
            Self::Off => None,
            Self::Adaptive => Some(Cow::Borrowed("thinking")),
            Self::Effort(level) => Some(Cow::Owned(format!("thinking: {level}"))),
            Self::Budget(tokens) => Some(Cow::Owned(format!("thinking: {tokens}"))),
        }
    }
}

impl std::fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Adaptive => f.write_str("adaptive"),
            Self::Effort(level) => f.write_str(level),
            Self::Budget(tokens) => write!(f, "{tokens}"),
        }
    }
}

impl From<StoredThinking> for ThinkingConfig {
    fn from(stored: StoredThinking) -> Self {
        match stored {
            StoredThinking::Off => Self::Off,
            StoredThinking::Adaptive => Self::Adaptive,
            StoredThinking::Effort { level } => Self::Effort(level.into_boxed_str()),
            StoredThinking::Budget { tokens } => Self::Budget(tokens),
        }
    }
}

impl From<ThinkingConfig> for StoredThinking {
    fn from(config: ThinkingConfig) -> Self {
        match config {
            ThinkingConfig::Off => Self::Off,
            ThinkingConfig::Adaptive => Self::Adaptive,
            ThinkingConfig::Effort(level) => Self::Effort {
                level: level.into_string(),
            },
            ThinkingConfig::Budget(tokens) => Self::Budget { tokens },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestOptions {
    pub thinking: ThinkingConfig,
    /// Raw user preference, reconciled by [`RequestOptions::clamped`] before use.
    pub fast: bool,
}

impl RequestOptions {
    /// Reconciles options with the model's capabilities. Called once before
    /// every request so UI state, restored sessions, and subagent flags all go
    /// through the same gate. A model that reasons unconditionally is handled
    /// in [`ThinkingConfig::resolve`], which knows what it declared.
    pub fn clamped(&self, model: &crate::model::Model) -> Self {
        Self {
            thinking: if model.supports_thinking() {
                self.thinking.clone()
            } else {
                ThinkingConfig::Off
            },
            fast: self.fast && model.supports_fast(),
        }
    }
}

pub type ToolNameAliases = Arc<HashMap<String, String>>;

#[derive(Debug, Default)]
pub struct StreamResponse {
    pub message: Message,
    pub usage: TokenUsage,
    pub stop_reason: Option<StopReason>,
    pub tool_name_aliases: Option<ToolNameAliases>,
}

/// Provider-reported usage quota, independent of local token accounting. Not every
/// provider exposes a programmatic quota endpoint; check `Provider::fetch_usage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// Subscription/plan level when the provider reports one (e.g. "lite").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub limits: Vec<UsageLimit>,
}

/// A single quota window (e.g. a 5-hour or weekly token quota).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Human-readable label for the window, provided by the provider.
    pub label: String,
    /// Usage percentage within the window, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u32>,
    /// When the window resets, as epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
    /// Extra provider-supplied context, e.g. "$2.33 spent" for usage credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::model::ThinkingSupport as Support;
    use test_case::test_case;

    #[test_case("end_turn", StopReason::EndTurn   ; "end_turn")]
    #[test_case("tool_use", StopReason::ToolUse   ; "tool_use")]
    #[test_case("max_tokens", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_anthropic(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_anthropic(input), expected);
    }

    #[test_case("stop", StopReason::EndTurn       ; "stop_maps_to_end_turn")]
    #[test_case("tool_calls", StopReason::ToolUse ; "tool_calls_maps_to_tool_use")]
    #[test_case("length", StopReason::MaxTokens   ; "length_maps_to_max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_openai(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_openai(input), expected);
    }

    #[test]
    fn user_with_images_text_and_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images("hello".into(), vec![source]);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
        assert!(matches!(&msg.content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn user_with_images_empty_text_only_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images(String::new(), vec![source]);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
    }

    #[test]
    fn message_kind_is_backward_compatible() {
        let old: Message = serde_json::from_value(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "hello" }]
        }))
        .unwrap();
        assert_eq!(old.kind, MessageKind::Turn);

        let turn = serde_json::to_value(Message::user("hello".into())).unwrap();
        assert!(turn.get("kind").is_none());

        let observation = Message::observation("built".into());
        assert_eq!(observation.first_user_text(), None);
        let observation = serde_json::to_value(observation).unwrap();
        assert_eq!(observation["kind"], "observation");
    }

    #[test]
    fn tool_output_ref_is_not_part_of_public_message_json() {
        let output_ref = ToolOutputRef {
            id: caudra_storage::id::CaudraId::generate()
                .to_string()
                .parse()
                .unwrap(),
            byte_count: 12,
            line_count: 2,
        };
        let block = ContentBlock::ToolResult {
            tool_use_id: "call-1".into(),
            content: "result".into(),
            is_error: false,
            output_ref: Some(output_ref),
        };

        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("output_ref").is_none());
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(
            roundtrip,
            ContentBlock::ToolResult {
                output_ref: None,
                ..
            }
        ));
    }

    #[test_case(ImageMediaType::Png,  "image/png"  ; "png")]
    #[test_case(ImageMediaType::Jpeg, "image/jpeg" ; "jpeg")]
    #[test_case(ImageMediaType::Gif,  "image/gif"  ; "gif")]
    #[test_case(ImageMediaType::Webp, "image/webp" ; "webp")]
    fn image_source_data_url(media: ImageMediaType, mime: &str) {
        let source = ImageSource::new(media, Arc::from("dGVzdA=="));
        assert_eq!(source.to_data_url(), format!("data:{mime};base64,dGVzdA=="));
    }

    #[test_case("image/png",  Some(ImageMediaType::Png)  ; "png")]
    #[test_case("image/webp", Some(ImageMediaType::Webp) ; "webp")]
    #[test_case("image/bmp",  None                       ; "unsupported")]
    fn media_type_from_mime(mime: &str, expected: Option<ImageMediaType>) {
        assert_eq!(ImageMediaType::from_mime(mime), expected);
    }

    #[test]
    fn adapt_images_borrows_when_model_has_vision_or_no_images() {
        let model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        let with_image = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, Arc::from("abc123")),
            }],
            ..Default::default()
        }];
        assert!(matches!(
            adapt_images_for_model(&model, &with_image),
            Cow::Borrowed(_)
        ));

        let mut text_only_model = model;
        text_only_model.supports_vision_override = Some(false);
        let no_images = vec![Message::user("hi".into())];
        assert!(matches!(
            adapt_images_for_model(&text_only_model, &no_images),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn adapt_images_replaces_blocks_for_text_only_model() {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.supports_vision_override = Some(false);
        let messages = vec![Message {
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
        let adapted = adapt_images_for_model(&model, &messages);
        assert_eq!(adapted[0].content.len(), 2);
        assert!(matches!(
            &adapted[0].content[0],
            ContentBlock::ToolResult { .. }
        ));
        assert!(
            matches!(&adapted[0].content[1], ContentBlock::Text { text } if text == IMAGE_OMITTED_NOTE)
        );
    }

    #[test]
    fn image_source_serde_injects_type_base64() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "base64");
        assert_eq!(json["media_type"], "image/png");
        assert_eq!(json["data"], "abc123");
        let deserialized: ImageSource = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.media_type, ImageMediaType::Png);
        assert_eq!(&*deserialized.data, "abc123");
    }

    fn effort(level: &str) -> ThinkingConfig {
        ThinkingConfig::Effort(level.into())
    }

    /// `max_output_tokens: 8192`, so the budget ceiling is 4096 unless the model
    /// declares a tighter one.
    fn thinking_model(id: &str) -> crate::model::Model {
        crate::model::Model {
            id: id.into(),
            ..clamp_test_model(crate::provider::ProviderKind::Anthropic)
        }
    }

    fn effort_model(levels: &[&str]) -> crate::model::Model {
        let mut model = thinking_model("test-model");
        model.reasoning_options = ReasoningOptions::new(vec![ReasoningOption::Effort {
            values: levels.iter().map(|level| (*level).to_string()).collect(),
        }]);
        model
    }

    fn budget_model(min: Option<u32>, max: Option<u32>) -> crate::model::Model {
        let mut model = thinking_model("test-model");
        model.reasoning_options =
            ReasoningOptions::new(vec![ReasoningOption::BudgetTokens { min, max }]);
        model
    }

    fn native_thinking_model(id: &str, fields: Value) -> crate::model::Model {
        let mut model = thinking_model(id);
        model.thinking_fields = Some(Box::new(serde_json::from_value(fields).unwrap()));
        model
    }

    fn native_effort_model() -> crate::model::Model {
        native_thinking_model(
            "local-model",
            json!({
                "off": {"reasoning_effort": "none"},
                "adaptive": {"reasoning_effort": "medium"},
                "low": {"reasoning_effort": "low"},
                "medium": {"reasoning_effort": "medium"},
                "xhigh": {"reasoning_effort": "xhigh"}
            }),
        )
    }

    #[test_case(ThinkingConfig::Off, "claude-opus-4-5", json!({}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-5", json!({"thinking": {"type": "adaptive"}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}) ; "budget_legacy_in_range")]
    #[test_case(ThinkingConfig::Off, "claude-opus-4-7", json!({}) ; "off_adaptive_model")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}}) ; "adaptive_adaptive_model")]
    #[test_case(effort("low"), "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "low"}}) ; "effort_low_passthrough")]
    #[test_case(effort("high"), "claude-opus-4-8-1m", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "effort_adaptive_opus_4_8_long_context")]
    #[test_case(effort("high"), "claude-opus-4.7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "effort_adaptive_copilot_dotted_id")]
    #[test_case(effort("high"), "anthropic/claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "effort_adaptive_gateway_prefixed_id")]
    #[test_case(ThinkingConfig::Budget(2048), "claude-3-5-sonnet-20241022", json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}) ; "budget_legacy_dated_id")]
    fn thinking_apply_to_body(config: ThinkingConfig, model_id: &str, expected: Value) {
        let mut body = json!({});
        config.apply_to_body(&mut body, &thinking_model(model_id));
        assert_eq!(body, expected);
    }

    const LEVELS_WITH_NONE: &[&str] = &["none", "low", "medium", "high"];
    const LEVELS_WITHOUT_NONE: &[&str] = &["low", "medium", "high", "xhigh"];
    const HIGH_ONLY: &[&str] = &["high"];

    #[test_case(LEVELS_WITH_NONE, ThinkingConfig::Off,       Some("none")  ; "declared_none_is_how_off_is_spelled")]
    #[test_case(LEVELS_WITH_NONE, ThinkingConfig::Adaptive,  None          ; "adaptive_leaves_the_choice_to_the_model")]
    #[test_case(LEVELS_WITH_NONE, effort("low"),             Some("low")   ; "declared_level_passes_through")]
    #[test_case(LEVELS_WITH_NONE, effort("minimal"),         Some("none")  ; "undeclared_below_floor_snaps_to_floor")]
    #[test_case(LEVELS_WITH_NONE, effort("max"),             Some("high")  ; "undeclared_above_top_snaps_to_top")]
    #[test_case(LEVELS_WITHOUT_NONE, ThinkingConfig::Off,    Some("low")   ; "no_declared_none_means_off_becomes_the_shallowest_level")]
    #[test_case(LEVELS_WITHOUT_NONE, effort("max"),          Some("xhigh") ; "max_snaps_to_declared_xhigh")]
    #[test_case(HIGH_ONLY, effort("minimal"),                Some("high")  ; "single_level_absorbs_everything")]
    #[test_case(HIGH_ONLY, ThinkingConfig::Budget(1024),     None          ; "budget_on_a_level_model_keeps_its_own_depth")]
    fn thinking_apply_reasoning_effort(
        levels: &[&str],
        config: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let mut body = json!({"model": "test"});
        config.apply_reasoning_effort(&mut body, &effort_model(levels));
        match expected {
            Some(e) => assert_eq!(body["reasoning_effort"], e),
            None => assert!(body.get("reasoning_effort").is_none()),
        }
    }

    #[test_case(ThinkingConfig::Off,           ResolvedThinking::Budget(2048) ; "off_becomes_the_shallowest_budget_when_it_cannot_be_disabled")]
    #[test_case(ThinkingConfig::Adaptive,      ResolvedThinking::On           ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(2048),  ResolvedThinking::Budget(2048) ; "budget_in_range")]
    #[test_case(ThinkingConfig::Budget(512),   ResolvedThinking::Budget(1024) ; "budget_floored_to_the_declared_min")]
    #[test_case(ThinkingConfig::Budget(10000), ResolvedThinking::Budget(4096) ; "budget_clamped_to_the_declared_max")]
    #[test_case(effort("max"),                 ResolvedThinking::Budget(4096) ; "top_of_the_ladder_is_the_ceiling")]
    #[test_case(effort("low"),                 ResolvedThinking::Budget(2048) ; "below_the_top_is_half_the_ceiling")]
    fn budget_model_resolves_against_declared_bounds(
        config: ThinkingConfig,
        expected: ResolvedThinking,
    ) {
        let model = budget_model(Some(1024), Some(4096));
        assert_eq!(config.resolve(&model), expected);
    }

    #[test_case(ThinkingConfig::Off, ReasoningOptions::default(), ResolvedThinking::Off ; "off_without_options")]
    #[test_case(ThinkingConfig::Off, ReasoningOptions::new(vec![ReasoningOption::Effort { values: vec!["none".into(), "high".into()] }]), ResolvedThinking::Effort("none".into()) ; "off_with_declared_none")]
    #[test_case(ThinkingConfig::Adaptive, ReasoningOptions::default(), ResolvedThinking::On ; "adaptive")]
    #[test_case(effort(" HIGH "), ReasoningOptions::new(vec![ReasoningOption::Effort { values: vec!["low".into(), "high".into()] }]), ResolvedThinking::Effort("high".into()) ; "normalized_declared_effort")]
    #[test_case(ThinkingConfig::Budget(2048), ReasoningOptions::new(vec![ReasoningOption::BudgetTokens { min: None, max: None }]), ResolvedThinking::Budget(2048) ; "declared_unbounded_budget")]
    fn thinking_resolve_exact_keeps_semantics(
        config: ThinkingConfig,
        reasoning_options: ReasoningOptions,
        expected: ResolvedThinking,
    ) {
        let mut model = thinking_model("exact-model");
        model.reasoning_options = reasoning_options;
        assert_eq!(config.resolve_exact(&model), Ok(expected));
    }

    #[test_case(effort("max"), effort_model(&["low", "high"]), ResolvedThinking::Effort("high".into()) ; "effort_snapping")]
    #[test_case(effort("max"), budget_model(Some(1024), Some(4096)), ResolvedThinking::Budget(4096) ; "effort_to_budget_translation")]
    #[test_case(ThinkingConfig::Budget(512), budget_model(Some(1024), Some(4096)), ResolvedThinking::Budget(1024) ; "budget_floor")]
    #[test_case(ThinkingConfig::Budget(8192), budget_model(Some(1024), Some(4096)), ResolvedThinking::Budget(4096) ; "budget_ceiling")]
    #[test_case(ThinkingConfig::Budget(2048), effort_model(&["low", "high"]), ResolvedThinking::On ; "budget_to_default_translation")]
    #[test_case(ThinkingConfig::Off, effort_model(&["low", "high"]), ResolvedThinking::Effort("low".into()) ; "required_thinking_promotion")]
    fn thinking_resolve_exact_rejects_semantic_changes(
        config: ThinkingConfig,
        model: crate::model::Model,
        best_effort: ResolvedThinking,
    ) {
        assert_eq!(
            config.resolve_exact(&model),
            Err(ThinkingCompatibilityError::Inexact(best_effort))
        );
    }

    #[test_case(effort("high") ; "effort_level")]
    #[test_case(ThinkingConfig::Budget(2048) ; "budget")]
    fn thinking_resolve_exact_requires_declared_options(config: ThinkingConfig) {
        let model = thinking_model("unknown-options-model");
        assert_eq!(
            config.resolve_exact(&model),
            Err(ThinkingCompatibilityError::UnknownOptions)
        );
    }

    #[test]
    fn thinking_resolve_exact_rejects_unsupported_thinking() {
        let mut model = thinking_model("unsupported-model");
        model.thinking_override = Some(Support::No);
        assert_eq!(
            ThinkingConfig::Adaptive.resolve_exact(&model),
            Err(ThinkingCompatibilityError::Unsupported)
        );
        assert_eq!(
            ThinkingConfig::Off.resolve_exact(&model),
            Ok(ResolvedThinking::Off)
        );
    }

    #[test]
    fn thinking_resolve_exact_requires_native_adaptive_support_for_claude() {
        let old_claude = thinking_model("claude-haiku-4-5");
        let legacy_claude = thinking_model("claude-3-7-sonnet-20250219");
        let adaptive_claude = thinking_model("claude-opus-4-7");

        assert_eq!(
            ThinkingConfig::Adaptive.resolve_exact(&old_claude),
            Err(ThinkingCompatibilityError::AdaptiveUnsupported)
        );
        assert_eq!(
            ThinkingConfig::Adaptive.resolve_exact(&legacy_claude),
            Err(ThinkingCompatibilityError::AdaptiveUnsupported)
        );
        assert_eq!(
            ThinkingConfig::Adaptive.resolve_exact(&adaptive_claude),
            Ok(ResolvedThinking::On)
        );
    }

    /// llama.cpp models declare no window and no bounds, so the request must
    /// still carry something honest rather than a number caudra invented.
    #[test_case(ThinkingConfig::Budget(16384), ResolvedThinking::Budget(16_384) ; "explicit_budget_passes_through")]
    #[test_case(ThinkingConfig::Budget(512),   ResolvedThinking::Budget(1024)   ; "protocol_floor_still_applies")]
    #[test_case(effort("max"),                 ResolvedThinking::Budget(32_768) ; "top_of_the_ladder_uses_the_fallback_ceiling")]
    fn undeclared_budget_model_falls_back(config: ThinkingConfig, expected: ResolvedThinking) {
        let mut model = budget_model(None, None);
        model.max_output_tokens = None;
        assert_eq!(config.resolve(&model), expected);
    }

    #[test_case(ThinkingConfig::Off,           json!({})                                                                    ; "off")]
    #[test_case(ThinkingConfig::Adaptive,      json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}})    ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096),  json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 4096}}}) ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000), json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 4096}}}) ; "budget_clamped_to_the_output_window")]
    fn thinking_apply_google_thinking(config: ThinkingConfig, expected: Value) {
        let mut model = budget_model(None, None);
        model.reasoning_options = ReasoningOptions::new(vec![
            ReasoningOption::Toggle,
            ReasoningOption::BudgetTokens {
                min: None,
                max: None,
            },
        ]);
        let mut body = json!({});
        config.apply_google_thinking(&mut body, &model);
        assert_eq!(body, expected);
    }

    /// Gemini 3 takes a level instead of a token count.
    #[test]
    fn google_effort_model_sends_thinking_level() {
        let model = effort_model(&["low", "medium", "high"]);
        let mut body = json!({});
        effort("high").apply_google_thinking(&mut body, &model);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingLevel": "high"})
        );
    }

    #[test_case(ThinkingConfig::Off,            0    ; "off")]
    #[test_case(ThinkingConfig::Adaptive,       -1   ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096),   4096 ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000),  4096 ; "budget_clamped_to_the_output_window")]
    fn thinking_apply_local_thinking(config: ThinkingConfig, expected: i64) {
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &thinking_model("local-model"));
        assert_eq!(body["thinking_budget_tokens"], expected);
    }

    #[test_case(ThinkingConfig::Off,      json!({"reasoning_effort": "none"})   ; "off")]
    #[test_case(ThinkingConfig::Adaptive, json!({"reasoning_effort": "medium"}) ; "adaptive")]
    #[test_case(effort("low"),            json!({"reasoning_effort": "low"})    ; "low")]
    #[test_case(effort("high"),           json!({"reasoning_effort": "medium"}) ; "undeclared_high_snaps_down")]
    #[test_case(effort("xhigh"),          json!({"reasoning_effort": "xhigh"})  ; "xhigh")]
    fn local_native_effort_uses_declared_levels(config: ThinkingConfig, expected: Value) {
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &native_effort_model());
        assert_eq!(body, expected);
    }

    #[test]
    fn local_required_thinking_maps_off_to_lowest_native_effort() {
        let mut model = native_effort_model();
        model.thinking_override = Some(Support::Required);
        let thinking = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: false,
        }
        .clamped(&model)
        .thinking;
        let mut body = json!({});
        thinking.apply_local_thinking(&mut body, &model);
        assert_eq!(body, json!({"reasoning_effort": "low"}));
    }

    #[test_case(ThinkingConfig::Off,          json!({"chat_template_kwargs": {"enable_thinking": false, "keep": 1}}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "adaptive")]
    #[test_case(effort("high"), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "effort_without_levels_uses_adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}, "thinking_budget_tokens": 2048}) ; "numeric_budget")]
    fn local_native_toggle_merges_into_nested_object(config: ThinkingConfig, expected: Value) {
        let model = native_thinking_model(
            "local-toggle-model",
            json!({
                "off": {"chat_template_kwargs": {"enable_thinking": false}},
                "adaptive": {"chat_template_kwargs": {"enable_thinking": true}}
            }),
        );
        let mut body = json!({"chat_template_kwargs": {"keep": 1}});
        config.apply_local_thinking(&mut body, &model);
        assert_eq!(body, expected);
    }

    /// A mode the model has no fragment for must still reach the server, so
    /// the budget field takes over instead of the request saying nothing.
    #[test_case(json!({"low": {"reasoning_effort": "low"}}), ThinkingConfig::Off, 0 ; "off_without_off")]
    #[test_case(json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Off, 0 ; "toggle_without_off")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Adaptive, -1 ; "adaptive_without_adaptive")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Budget(4096), 4096 ; "budget_without_levels")]
    fn local_native_missing_fragment_falls_back_to_budget(
        fields: Value,
        config: ThinkingConfig,
        expected: i64,
    ) {
        let model = native_thinking_model("local-partial", fields);
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &model);
        assert_eq!(body, json!({ "thinking_budget_tokens": expected }));
    }

    /// llama.cpp models have no known output window; the budget the user
    /// asked for must reach the server untouched.
    #[test]
    fn local_thinking_unknown_window_passes_budget_through() {
        let mut model = thinking_model("llama-cpp-model");
        model.max_output_tokens = None;
        let mut body = json!({});
        ThinkingConfig::Budget(16_384).apply_local_thinking(&mut body, &model);
        assert_eq!(body["thinking_budget_tokens"], 16_384);
    }

    fn clamp_test_model(provider: crate::provider::ProviderKind) -> crate::model::Model {
        crate::model::Model {
            id: "test-model".into(),
            provider: std::sync::Arc::<str>::from(provider.to_string()),
            tier: crate::model::ModelTier::Medium,
            family: provider.family(),
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: Some(provider.family().supports_vision()),
            pricing: crate::model::ModelPricing::default(),
            discovered_free: false,
            max_output_tokens: Some(8192),
            context_window: 200_000,
            reasoning_options: ReasoningOptions::default(),
            thinking_fields: None,
        }
    }

    #[test_case(None,                    ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "provider_default_keeps")]
    #[test_case(Some(Support::No),       ThinkingConfig::Adaptive, ThinkingConfig::Off             ; "unsupported_clamps_off")]
    #[test_case(Some(Support::Yes),      ThinkingConfig::Off,      ThinkingConfig::Off             ; "supported_keeps_off")]
    #[test_case(Some(Support::Required), ThinkingConfig::Off,      ThinkingConfig::Off             ; "required_off_is_resolved_at_request_time")]
    #[test_case(Some(Support::Required), ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "required_keeps_enabled")]
    fn request_options_clamped_thinking(
        thinking_override: Option<Support>,
        thinking: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.thinking_override = thinking_override;
        let opts = RequestOptions {
            thinking,
            fast: false,
        };
        assert_eq!(opts.clamped(&model).thinking, expected);
    }

    #[test]
    fn request_options_clamped_fast_requires_model_support() {
        let model = clamp_test_model(crate::provider::ProviderKind::Google);
        let opts = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: true,
        };
        assert!(!opts.clamped(&model).fast);
    }

    #[test_case("",         ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "toggle_on")]
    #[test_case("",         ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "toggle_off")]
    #[test_case("off",      ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "explicit_off")]
    #[test_case("adaptive", ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "explicit_adaptive")]
    #[test_case("high",     ThinkingConfig::Off,      Ok(effort("high")) ; "explicit_effort")]
    #[test_case("8192",     ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(8192)) ; "explicit_budget")]
    #[test_case("512",      ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(512)) ; "small_budget")]
    #[test_case("0",        ThinkingConfig::Off,      Err(())                       ; "budget_zero")]
    #[test_case("garbage",  ThinkingConfig::Off,      Err(())                       ; "invalid_input")]
    fn thinking_parse(input: &str, current: ThinkingConfig, expected: Result<ThinkingConfig, ()>) {
        let result = ThinkingConfig::parse(input, &current).map_err(|_| ());
        assert_eq!(result, expected);
    }

    #[test_case(ThinkingConfig::Off      ; "off")]
    #[test_case(ThinkingConfig::Adaptive ; "adaptive")]
    #[test_case(effort("max") ; "effort_level")]
    #[test_case(ThinkingConfig::Budget(8192) ; "budget")]
    fn thinking_display_round_trip(config: ThinkingConfig) {
        let s = config.to_string();
        let parsed = ThinkingConfig::parse(&s, &ThinkingConfig::Off).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn thinking_serde_no_signature_omits_field() {
        let block = ContentBlock::Thinking {
            thinking: "x".into(),
            signature: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("signature").is_none());
    }
}
