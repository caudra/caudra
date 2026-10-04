pub(crate) mod error;
pub mod history;
pub mod manifest;
pub mod model;
pub mod model_registry;
pub mod pricing;
pub mod provider;
pub(crate) mod providers;
/// Names for auth lifecycle events, shared so the three OAuth providers report
/// the same shape under `caudra::provider`.
pub mod auth_events {
    pub const REFRESHED: &str = "auth_refreshed";
}

pub mod retry;
pub mod tokens;
pub(crate) mod types;

pub use caudra_storage::sessions::add_cost;
pub use error::AgentError;
pub use history::{
    AssistantTextState, CaudraId, HistoryItem, HistoryItemKind, HistoryProjectionError, UserOrigin,
    active_history_items, expand_message, merge_history_items, project_messages,
    resolve_history_head, transcript_history_items, validate_history_items,
};
pub use model::{
    Billing, FastPricing, Model, ModelEntry, ModelError, ModelFacts, ModelFamily, ModelGeneration,
    ModelInfo, ModelMarker, ModelPricing, ModelPurpose, PricingTier, StaticReasoningOption,
    ThinkingSupport, TokenUsage, format_tokens,
};
pub use pricing::{ModelSpend, SessionSpend, model_cost, settle_session};
pub use provider::WireRequest;
pub use providers::Timeouts;
pub use providers::anthropic::auth as anthropic_auth;
pub use providers::catalog::ProviderData;
pub use providers::catalog::{
    catalog_provider, catalog_provider_if_available, catalog_providers,
    catalog_providers_if_available, model_meta_if_available, warm_catalog,
};
pub use providers::copilot::auth as copilot_auth;
pub use providers::dynamic;
pub use providers::openai::auth as openai_auth;
pub use providers::openai::images as openai_images;
pub use providers::xai::auth as xai_auth;
pub use tokens::{
    estimate_tokens, estimate_tokens_cached, format_hit_rate, format_tokens_u64, token_label,
};
pub use types::{
    CacheKey, ContentBlock, EFFORT_LEVELS, EMPTY_RESPONSE_MARKER, IMAGE_OMITTED_NOTE,
    INVALID_TOOL_JSON_KEY, ImageMediaType, ImageSource, InvalidToolInput, MAX_TOOL_INPUT_BYTES,
    MIN_THINKING_BUDGET, Message, MessageKind, PEER_SCRIPT_SENDER, PEER_SESSION_SENDER,
    PeerAssignment, PeerAudience, PeerMessageOrigin, ProviderEvent, ProviderUsage, ReasoningOption,
    ReasoningOptions, ReasoningSource, ReasoningTransport, RequestOptions, ResolvedThinking,
    ResponsesReasoning, Role, StandingReminderKind, SteeringKind, SteeringOrigin, StopReason,
    StreamResponse, THINKING_USAGE, TaskEventOrigin, ThinkingConfig, ToolNameAliases, UsageLimit,
    WorkflowEventOrigin, adapt_images_for_model, invalid_tool_input,
};
