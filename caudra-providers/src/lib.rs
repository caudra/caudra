pub(crate) mod error;
pub mod history;
pub mod manifest;
pub mod model;
pub mod model_registry;
pub mod pricing;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub mod tokens;
pub(crate) mod types;

pub use caudra_storage::sessions::add_cost;
pub use error::AgentError;
pub use history::{
    AssistantTextState, CaudraId, HistoryItem, HistoryItemKind, HistoryProjectionError, UserOrigin,
    active_history_items, expand_message, merge_history_items, project_messages,
    resolve_history_head,
};
pub use model::{
    Billing, FastPricing, Model, ModelEntry, ModelError, ModelFamily, ModelInfo, ModelPricing,
    ModelTier, PricingTier, StaticReasoningOption, ThinkingSupport, TokenUsage, format_tokens,
};
pub use pricing::{ModelSpend, SessionSpend, model_cost, settle_session};
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
pub use tokens::{estimate_tokens, estimate_tokens_cached, format_tokens_u64, token_label};
pub use types::{
    ContentBlock, EFFORT_LEVELS, EMPTY_RESPONSE_MARKER, IMAGE_OMITTED_NOTE, ImageMediaType,
    ImageSource, MIN_THINKING_BUDGET, Message, MessageKind, ProviderEvent, ProviderUsage,
    ReasoningOption, ReasoningOptions, ReasoningSource, ReasoningTransport, RequestOptions,
    ResolvedThinking, ResponsesReasoning, Role, StopReason, StreamResponse, THINKING_USAGE,
    ThinkingConfig, ToolNameAliases, UsageLimit, adapt_images_for_model,
};
