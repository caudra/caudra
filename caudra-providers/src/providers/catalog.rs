//! Snapshot catalog metadata and provider dispatch.
//!
//! Per-slug providers (e.g. a user who configures `nvidia/...` directly) get
//! their own [`CatalogProvider`] instance, created from the same
//! [`ProviderData`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use caudra_config::providers::{ProvidersConfig, builtin_provider};
use caudra_models::{ALLOWED_NPM, BLOCKED_CATALOG_PROVIDERS};
use flume::Sender;
use isahc::Request;
use serde_json::Value;
use tracing::debug;

use caudra_storage::StateDir;
use caudra_storage::accounts::{StoredSecret, list_accounts};

use caudra_storage::thinking::ReasoningOptions;

use crate::accounts::AccountCredential;
use crate::model::{Model, ModelInfo, ModelPricing, PricingTier};
use crate::model_data::current;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::anthropic::{self, shared};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, Timeouts};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

const MESSAGES_PATH: &str = "/messages";

pub(crate) const OPENCODE_FAMILY_SLUGS: &[&str] = &["opencode", "opencode-go"];

/// The token Opencode accepts for its free models when no key is set.
const PUBLIC_FREE_KEY: &str = "public";

/// Used only when a catalog entry omits its limits entirely.
const FALLBACK_CATALOG_CONTEXT: u32 = 128_000;
const FALLBACK_CATALOG_OUTPUT: u32 = 64_000;

const FREE_MODELS_OPT_IN: &str = "providers.opencode.enable_free_models = true";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    ChatCompletions,
    Messages,
}

/// Provider metadata from the catalog, exposed for the login UI.
#[derive(Clone, Debug)]
pub struct ProviderData {
    pub slug: String,
    pub display_name: String,
    /// Environment variable names for API keys
    pub env_keys: Vec<String>,
    /// API base URL
    pub base_url: Option<String>,
    /// NPM package name. Used to determine how to interact with the model.
    pub npm: String,
    /// API format (ChatCompletions or Messages)
    pub api_format: EndpointType,
    /// Models for this provider
    pub models: HashMap<String, CatalogMeta>,
}

fn is_free_model(meta: &CatalogMeta) -> bool {
    meta.input_price == 0.0 && meta.output_price == 0.0
}

impl ProviderData {
    pub(crate) fn new(
        slug: String,
        catalog_provider: &schema::CatalogProvider,
        api_format: EndpointType,
        models: HashMap<String, CatalogMeta>,
    ) -> Self {
        Self {
            slug,
            display_name: catalog_provider.name.clone(),
            env_keys: catalog_provider.env.clone(),
            base_url: catalog_provider.api.clone(),
            npm: catalog_provider.npm.clone(),
            api_format,
            models,
        }
    }

    /// The key of the first stored account, for discovery and listings. A
    /// provider instance signs with the account it was built for instead.
    pub fn load_key_from_storage(&self, state_dir: &StateDir) -> Option<String> {
        list_accounts(state_dir, &self.slug)
            .ok()?
            .into_iter()
            .find_map(|(_, account)| match account.secret {
                StoredSecret::ApiKey(credentials) => Some(credentials.api_key),
                StoredSecret::OAuth(_) => None,
            })
    }

    pub fn resolve_api_key(&self, state_dir: &StateDir) -> Option<String> {
        for var in &self.env_keys {
            if let Ok(val) = std::env::var(var) {
                debug!(provider = %self.display_name, var = %var, "api key resolved from env");
                return Some(val);
            }
        }
        if let Some(key) = self.load_key_from_storage(state_dir) {
            debug!(provider = %self.display_name, "api key resolved from storage");
            return Some(key);
        }
        None
    }

    pub fn env_key_set(&self) -> Option<&str> {
        self.env_keys
            .iter()
            .find(|e| std::env::var(e).is_ok())
            .map(|s| s.as_str())
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        match self.npm.as_str() {
            "@ai-sdk/anthropic" => vec![("x-api-key".into(), api_key.into())],
            _ => vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    pub fn build_auth(&self, state_dir: &StateDir) -> Authentication {
        self.auth_for_key(self.resolve_api_key(state_dir).as_deref())
    }

    /// What `api_key` signs with here; without one only the Opencode family
    /// still answers, on its public free-models token.
    pub(crate) fn auth_for_key(&self, api_key: Option<&str>) -> Authentication {
        match api_key {
            Some(key) => Authentication::KeyBased(ResolvedAuth {
                base_url: self.base_url.clone(),
                headers: self.auth_headers(key),
            }),
            None if OPENCODE_FAMILY_SLUGS.contains(&self.slug.as_str()) => {
                Authentication::OpenCodeFreeKey(ResolvedAuth {
                    base_url: self.base_url.clone(),
                    headers: self.auth_headers(PUBLIC_FREE_KEY),
                })
            }
            None => Authentication::NoAuth,
        }
    }

    pub fn resolve_auth(&self, state_dir: &StateDir) -> Option<ResolvedAuth> {
        self.build_auth(state_dir).into_resolved()
    }

    /// Resolve auth with optional dynamic override (e.g. from Lua).
    pub fn resolve_auth_with_override(
        &self,
        override_auth: Option<&Arc<Mutex<ResolvedAuth>>>,
        state_dir: &StateDir,
    ) -> Option<ResolvedAuth> {
        if OPENCODE_FAMILY_SLUGS.contains(&self.slug.as_str())
            && let Some(auth) = override_auth
        {
            return Some(auth.lock().unwrap().clone());
        }
        self.resolve_auth(state_dir)
    }

    pub fn available_models(
        &self,
        state_dir: &StateDir,
        enable_free_models: bool,
    ) -> Vec<ModelInfo> {
        let auth = self.build_auth(state_dir);
        let mut models: Vec<ModelInfo> = self
            .models
            .iter()
            .filter_map(|(model_id, meta)| {
                let is_free = is_free_model(meta);
                if is_free && !enable_free_models {
                    return None;
                }
                let allow_model = match &auth {
                    Authentication::KeyBased(_) => true,
                    Authentication::OpenCodeFreeKey(_) => is_free,
                    Authentication::NoAuth => false,
                };
                if !allow_model {
                    return None;
                }
                Some(meta.model_info(model_id))
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogMeta {
    pub context: u32,
    /// Whether [`context`](Self::context) came from `limit.input`, which grants
    /// the output allowance on top of it rather than carving it out.
    pub context_excludes_output: bool,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub supports_thinking: bool,
    pub supports_vision: bool,
    pub reasoning_options: ReasoningOptions,
    pub pricing_tiers: Vec<PricingTier>,
}

impl CatalogMeta {
    pub(crate) fn from_model(model: &schema::CatalogModel) -> Self {
        let cost = model.cost.as_ref();
        let limit = model.limit.as_ref();
        Self {
            // `input` is the real prompt budget where the catalog distinguishes
            // it, and `context` counts the output allowance too. Caudra measures
            // a conversation against this, so taking the total would overstate
            // what fits by however much the model may reply.
            context: limit
                .and_then(|l| l.input.or(l.context))
                .unwrap_or(FALLBACK_CATALOG_CONTEXT),
            context_excludes_output: limit.is_some_and(|l| l.input.is_some()),
            output: limit
                .and_then(|l| l.output)
                .unwrap_or(FALLBACK_CATALOG_OUTPUT),
            input_price: cost.and_then(|c| c.input).unwrap_or(0.0),
            output_price: cost.and_then(|c| c.output).unwrap_or(0.0),
            cache_read: cost.and_then(|c| c.cache_read).unwrap_or(0.0),
            cache_write: cost.and_then(|c| c.cache_write).unwrap_or(0.0),
            supports_thinking: model.reasoning,
            supports_vision: model.attachment
                || model
                    .modalities
                    .as_ref()
                    .is_some_and(|m| m.input.iter().any(|kind| kind == "image")),
            reasoning_options: model.reasoning_options.clone().unwrap_or_default(),
            pricing_tiers: cost
                .map(|c| {
                    // A rate a tier leaves out is the base rate, not a free one.
                    let mut tiers: Vec<PricingTier> = c
                        .tiers
                        .iter()
                        .map(|tier| PricingTier {
                            above: tier.tier.size,
                            input: tier.input.or(c.input).unwrap_or(0.0),
                            output: tier.output.or(c.output).unwrap_or(0.0),
                            cache_read: tier.cache_read.or(c.cache_read).unwrap_or(0.0),
                            cache_write: tier.cache_write.or(c.cache_write).unwrap_or(0.0),
                            fast: None,
                        })
                        .collect();
                    tiers.sort_by_key(|tier| tier.above);
                    tiers
                })
                .unwrap_or_default(),
        }
    }

    fn pricing(&self) -> ModelPricing {
        ModelPricing {
            input: self.input_price,
            output: self.output_price,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            fast: None,
            tiers: Cow::Owned(self.pricing_tiers.clone()),
        }
    }

    fn model_info(&self, model_id: &str) -> ModelInfo {
        ModelInfo {
            id: model_id.to_string(),
            context_window: Some(self.context),
            window_excludes_output: self.context_excludes_output,
            max_output_tokens: Some(self.output),
            pricing: Some(self.pricing()),
            supports_thinking: Some(self.supports_thinking),
            supports_vision: Some(self.supports_vision),
            reasoning_options: Some(self.reasoning_options.clone()),
            provider_info: None,
        }
    }
}

#[derive(Clone)]
pub enum Authentication {
    /// User has a configured API key — all models accessible
    KeyBased(ResolvedAuth),
    /// No real key configured — only free/zero-cost models via OpenCode public
    /// fallback
    OpenCodeFreeKey(ResolvedAuth),
    /// No authentication available
    NoAuth,
}

impl Authentication {
    pub(crate) fn into_resolved(self) -> Option<ResolvedAuth> {
        match self {
            Self::KeyBased(auth) | Self::OpenCodeFreeKey(auth) => Some(auth),
            Self::NoAuth => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogData {
    /// Providers caudra will route requests through. Filtered hard: unsupported
    /// SDK shapes, blocked ids, and anything a built-in already serves.
    providers: HashMap<String, ProviderData>,
    /// What the catalog knows about every model, including the ones routed by a
    /// built-in provider. Routing eligibility and model facts are different
    /// questions, and only the first justifies a block list.
    metadata: HashMap<String, HashMap<String, CatalogMeta>>,
}

impl CatalogData {
    pub(crate) fn same_route(&self, other: &Self, slug: &str, id: &str) -> bool {
        match (self.providers.get(slug), other.providers.get(slug)) {
            (Some(previous), Some(next)) => {
                previous.base_url == next.base_url
                    && previous.env_keys == next.env_keys
                    && previous.npm == next.npm
                    && previous.models.get(id) == next.models.get(id)
            }
            (None, None) => true,
            _ => false,
        }
    }
    pub(crate) fn provider(&self, slug: &str) -> Option<ProviderData> {
        self.providers.get(slug).cloned()
    }
    pub(crate) fn from_index(index: schema::CatalogIndex) -> Self {
        let mut providers = HashMap::new();
        let mut metadata = HashMap::new();

        for (provider_id, provider) in index {
            let models: HashMap<String, CatalogMeta> = provider
                .models
                .iter()
                .map(|(model_id, model)| (model_id.clone(), CatalogMeta::from_model(model)))
                .collect();
            let model_count = models.len();

            // Every provider contributes its model facts, whatever caudra does
            // about routing: the built-ins below need them most, and they are
            // exactly the ones routing turns away.
            metadata.insert(provider_id.clone(), models.clone());

            if !ALLOWED_NPM.contains(&provider.npm.as_str()) {
                debug!(npm = %provider.npm, "skipping provider: unsupported npm package");
                continue;
            }
            if BLOCKED_CATALOG_PROVIDERS.contains(&provider_id.as_str()) {
                debug!(
                    provider = &provider_id,
                    "skipping providers from the catalog"
                );
                continue;
            }

            let Some(_base_url) = &provider.api else {
                debug!(provider = %provider_id, "skipping: no API URL in catalog");
                continue;
            };

            if builtin_provider(&provider_id).is_some()
                && !OPENCODE_FAMILY_SLUGS.contains(&provider_id.as_str())
            {
                debug!(
                    provider = &provider_id,
                    "skipping providers supported by built-in providers"
                );
                continue;
            }

            let api_format = determine_catalog_format(&provider.npm);
            let provider_data =
                ProviderData::new(provider_id.clone(), &provider, api_format, models);
            providers.insert(provider_id.clone(), provider_data);

            debug!(
                provider = %provider_id,
                models = model_count,
                format = %provider.npm,
                "catalog provider registered",
            );
        }

        Self {
            providers,
            metadata,
        }
    }

    fn meta(&self, provider: &str, model_id: &str) -> Option<&CatalogMeta> {
        self.metadata.get(provider)?.get(model_id)
    }

    fn model_ids(&self, provider: &str) -> Vec<String> {
        self.metadata
            .get(provider)
            .map(|models| models.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn lookup(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<(&CatalogMeta, &ProviderData), AgentError> {
        let provider_data = self
            .providers
            .get(provider)
            .ok_or_else(|| config_error(format!("provider '{provider}' not found in catalog")))?;
        let meta = provider_data.models.get(model_id).ok_or_else(|| {
            config_error(format!(
                "model '{provider}/{model_id}' not found in catalog"
            ))
        })?;
        Ok((meta, provider_data))
    }

    fn all_providers(&self) -> Vec<ProviderData> {
        let mut providers: Vec<ProviderData> = self.providers.values().cloned().collect();
        providers.sort_by_key(|p| p.display_name.to_lowercase());
        providers
    }
}

pub(crate) fn config_error(message: String) -> AgentError {
    AgentError::Config { message }
}

static CATALOG_PROVIDER_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "catalog",
};

/// Returns the list of all providers in alphabetical order.
pub fn catalog_providers() -> Vec<ProviderData> {
    current().catalog.all_providers()
}

pub fn catalog_provider(provider_id: &str) -> Option<ProviderData> {
    current().catalog.providers.get(provider_id).cloned()
}

pub fn available(slug: &str) -> bool {
    let Some(data) = catalog_provider(slug) else {
        return false;
    };
    let Ok(state_dir) = StateDir::resolve() else {
        return false;
    };
    data.resolve_auth(&state_dir).is_some()
}

pub(crate) fn credential_env_vars(slug: &str) -> Option<Vec<String>> {
    catalog_provider(slug).map(|data| data.env_keys)
}

fn determine_catalog_format(npm: &str) -> EndpointType {
    match npm {
        "@ai-sdk/anthropic" => EndpointType::Messages,
        _ => EndpointType::ChatCompletions,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn catalog_request(
    chat_compat: &OpenAiCompatProvider,
    api_format: EndpointType,
    auth: &ResolvedAuth,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    thinking: &ThinkingConfig,
) -> WireRequest {
    match api_format {
        EndpointType::ChatCompletions => {
            let mut body = chat_compat.build_body(model, messages, system, tools);
            thinking.apply_reasoning_effort(&mut body, model);
            WireRequest::post(chat_compat.chat_url(auth), body)
        }
        EndpointType::Messages => WireRequest::post(
            format!(
                "{}{MESSAGES_PATH}",
                auth.base_url.as_deref().unwrap_or_default()
            ),
            shared::messages_body(model, &model.id, messages, None, system, tools, thinking),
        ),
    }
}

/// Sends what [`catalog_request`] built for the same `api_format`.
pub(crate) async fn send_catalog_request(
    chat_compat: &OpenAiCompatProvider,
    api_format: EndpointType,
    auth: &ResolvedAuth,
    model: &Model,
    wire: &WireRequest,
    event_tx: &Sender<ProviderEvent>,
) -> Result<StreamResponse, AgentError> {
    match api_format {
        EndpointType::ChatCompletions => {
            chat_compat
                .do_stream(model, &[], wire, event_tx, auth)
                .await
        }
        EndpointType::Messages => {
            let request = auth
                .configure_request(
                    Request::builder()
                        .method(wire.method)
                        .uri(&wire.url)
                        .header("user-agent", super::user_agent())
                        .header("content-type", "application/json")
                        .header("anthropic-version", anthropic::API_VERSION),
                )
                .body(serde_json::to_vec(&wire.body)?)?;
            debug!(model = %model.id, "sending Anthropic-format request via catalog");
            let response = chat_compat.client().send_async(request).await?;
            if response.status().as_u16() == 200 {
                anthropic::parse_sse(response, event_tx, chat_compat.stream_timeout()).await
            } else {
                Err(AgentError::from_response(response).await)
            }
        }
    }
}

/// `Provider` for a single catalog sub-provider. Created with a resolved
/// `ProviderData` (from `caudra_providers::catalog_provider(slug)`) plus the
/// auth the catalog names for that sub-provider.
pub struct CatalogProvider {
    data: ProviderData,
    auth: CatalogAuth,
    chat_compat: OpenAiCompatProvider,
}

/// Which models the resolved auth unlocks: a real key unlocks all, the
/// no-key `enable_free_models` opt-in unlocks free models only, and `Gated`
/// unlocks nothing. `Gated` holds no credentials at all, so it can never send
/// the public token by accident: discovery lists nothing, and only an actual
/// attempt to stream tells the user to log in or opt in.
enum CatalogAuth {
    Keyed(ResolvedAuth),
    FreeOnly(ResolvedAuth),
    Gated,
}

impl CatalogProvider {
    pub fn new(
        data: ProviderData,
        account: &AccountCredential,
        timeouts: Timeouts,
        allow_free_fallback: bool,
    ) -> Result<Self, AgentError> {
        let auth = match data.auth_for_key(account.optional_api_key()) {
            Authentication::KeyBased(auth) => CatalogAuth::Keyed(auth),
            Authentication::OpenCodeFreeKey(auth) if allow_free_fallback => {
                CatalogAuth::FreeOnly(auth)
            }
            Authentication::OpenCodeFreeKey(_) => CatalogAuth::Gated,
            Authentication::NoAuth => {
                return Err(AgentError::Config {
                    message: format!(
                        "no API key configured for provider '{}'; run `caudra auth login {}`",
                        data.slug, data.slug
                    ),
                });
            }
        };
        Ok(Self {
            data,
            auth,
            chat_compat: OpenAiCompatProvider::new(&CATALOG_PROVIDER_CONFIG, timeouts),
        })
    }

    /// The turn a send carries and the key it goes with. `Gated` holds no key,
    /// so the dry run refuses exactly where the send would.
    fn prepare(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
    ) -> Result<(&ResolvedAuth, WireRequest), AgentError> {
        let auth = match &self.auth {
            CatalogAuth::Keyed(auth) | CatalogAuth::FreeOnly(auth) => auth,
            CatalogAuth::Gated => {
                let slug = &self.data.slug;
                return Err(AgentError::Config {
                    message: format!(
                        "provider '{slug}' has no API key; run `caudra auth login {slug}` or set {FREE_MODELS_OPT_IN} to use its free models"
                    ),
                });
            }
        };
        let wire = catalog_request(
            &self.chat_compat,
            self.data.api_format,
            auth,
            model,
            messages,
            system,
            tools,
            &opts.thinking,
        );
        Ok((auth, wire))
    }
}

impl Provider for CatalogProvider {
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
        Box::pin(async move {
            let (auth, wire) = self.prepare(model, messages, system, tools, &opts)?;
            send_catalog_request(
                &self.chat_compat,
                self.data.api_format,
                auth,
                model,
                &wire,
                event_tx,
            )
            .await
        })
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
        self.prepare(model, messages, system, tools, opts)
            .map(|(_, wire)| wire)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            Ok(self
                .data
                .models
                .iter()
                .filter(|(_, meta)| match &self.auth {
                    CatalogAuth::Keyed(_) => true,
                    CatalogAuth::FreeOnly(_) => is_free_model(meta),
                    CatalogAuth::Gated => false,
                })
                .map(|(model_id, meta)| meta.model_info(model_id))
                .collect())
        })
    }
}

pub(crate) fn unresolved_slug_reason(slug: &str) -> String {
    format!("unknown provider '{slug}'")
}

/// Zen hides free models behind `providers.opencode.enable_free_models`;
/// the no-key public-token fallback must honor the same opt-in.
pub(crate) fn free_fallback_allowed() -> bool {
    ProvidersConfig::load()
        .get("opencode")
        .and_then(|definition| definition.enable_free_models)
        .unwrap_or(false)
}

pub(crate) fn try_create(
    catalog: &CatalogData,
    slug: &str,
    account: &AccountCredential,
    timeouts: Timeouts,
) -> Option<Result<Box<dyn Provider>, AgentError>> {
    let data = catalog.provider(slug)?;
    Some(
        CatalogProvider::new(data, account, timeouts, free_fallback_allowed())
            .map(|provider| Box::new(provider) as Box<dyn Provider>),
    )
}

/// Reads metadata even for providers routed by a built-in transport.
pub fn model_meta(slug: &str, model_id: &str) -> Option<CatalogMetaView> {
    current()
        .catalog
        .meta(slug, model_id)
        .map(CatalogMetaView::from)
}

/// The ids the catalog describes, not necessarily what the caller may run.
pub fn model_ids(slug: &str) -> Vec<String> {
    current().catalog.model_ids(slug)
}

pub(crate) fn free_model(slug: &str, model_id: &str) -> bool {
    OPENCODE_FAMILY_SLUGS.contains(&slug)
        && current()
            .catalog
            .providers
            .get(slug)
            .and_then(|provider| provider.models.get(model_id))
            .is_some_and(is_free_model)
}

/// Metadata shape `Model::from_spec` consumes when a spec resolves to a catalog
/// sub-provider. Public so `caudra-providers/src/model.rs` can name it without
/// depending on the opencode-internal `CatalogMeta` struct.
#[derive(Debug, Clone)]
pub struct CatalogMetaView {
    pub context: u32,
    pub context_excludes_output: bool,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub supports_thinking: bool,
    pub supports_vision: bool,
    pub reasoning_options: ReasoningOptions,
    pub pricing_tiers: Vec<PricingTier>,
}

impl From<&CatalogMeta> for CatalogMetaView {
    fn from(meta: &CatalogMeta) -> Self {
        Self {
            context: meta.context,
            context_excludes_output: meta.context_excludes_output,
            output: meta.output,
            input_price: meta.input_price,
            output_price: meta.output_price,
            cache_read: meta.cache_read,
            cache_write: meta.cache_write,
            supports_thinking: meta.supports_thinking,
            supports_vision: meta.supports_vision,
            reasoning_options: meta.reasoning_options.clone(),
            pricing_tiers: meta.pricing_tiers.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ReasoningOptions;
    use std::collections::HashMap;

    use super::schema::{CatalogCost, CatalogIndex, CatalogLimits, CatalogModel, CatalogProvider};
    use std::sync::Arc;

    use caudra_storage::accounts::{
        AccountId, AccountIdentity, AccountName, StoredAccount, StoredSecret, save_account,
    };
    use caudra_storage::auth::ProviderCredentials;
    use serde_json::json;

    use super::{
        Authentication, CatalogData, CatalogMeta, EndpointType, FREE_MODELS_OPT_IN, ProviderData,
        StateDir, determine_catalog_format,
    };
    use crate::accounts::credential::test_support::{api_key_account, keyless_account};
    use crate::model::Model;
    use crate::provider::Provider;
    use crate::providers::Timeouts;
    use crate::providers::test_support::CREDENTIAL_IN_URL;
    use crate::{AgentError, ModelFamily, RequestOptions};
    use test_case::test_case;

    const OPENCODE_GO: &str = "opencode-go";
    const PAID_MODEL: &str = "paid-model";
    const FREE_MODEL: &str = "free-model";
    const MODEL_OUTPUT: u32 = 64_000;
    const UNSET_KEY_ENV: &str = "CAUDRA_TEST_OPENCODE_GO_UNSET_KEY_30571";
    const CATALOG_CREDENTIAL: &str = "sk-catalog-credential";
    const ACCOUNT: &str = "work";
    const SYSTEM_PROMPT: &str = "catalog system";
    const NOW: &str = "2026-10-29T12:00:00Z";
    const NVIDIA: &str = "nvidia";
    const NVIDIA_MODEL: &str = "z-ai/glm-5.3";
    const NVIDIA_URL: &str = "https://integrate.api.nvidia.com/v1/chat/completions";
    const UNKNOWN_PROVIDER: &str = "caudra-unknown-catalog-provider";
    const UNKNOWN_MESSAGE: &str = "unknown provider 'caudra-unknown-catalog-provider'";

    #[test]
    fn snapshot_provider_builds_a_request_without_a_first_send() {
        let provider = super::try_create(
            &crate::model_data::current().catalog,
            NVIDIA,
            &api_key_account(NVIDIA, ACCOUNT, CATALOG_CREDENTIAL),
            Timeouts::default(),
        )
        .unwrap()
        .unwrap();
        let model = Model::from_spec(&format!("{NVIDIA}/{NVIDIA_MODEL}")).unwrap();
        let wire = provider
            .wire_request(
                &model,
                &[],
                SYSTEM_PROMPT,
                &json!([]),
                &RequestOptions::default(),
                None,
            )
            .unwrap();
        assert_eq!(wire.url, NVIDIA_URL);
        assert_eq!(wire.body["model"], NVIDIA_MODEL);
    }

    #[test]
    fn unknown_snapshot_provider_is_rejected_at_construction() {
        assert!(
            super::try_create(
                &crate::model_data::current().catalog,
                UNKNOWN_PROVIDER,
                &keyless_account(UNKNOWN_PROVIDER),
                Timeouts::default(),
            )
            .is_none()
        );
        assert_eq!(
            super::unresolved_slug_reason(UNKNOWN_PROVIDER),
            UNKNOWN_MESSAGE
        );
        assert!(super::credential_env_vars(UNKNOWN_PROVIDER).is_none());
    }

    #[test]
    fn new_rejects_no_auth() {
        let data = ProviderData {
            slug: "test".into(),
            display_name: "Test".into(),
            env_keys: vec![],
            base_url: None,
            npm: "@ai-sdk/openai".into(),
            api_format: EndpointType::ChatCompletions,
            models: HashMap::new(),
        };
        let result =
            super::CatalogProvider::new(data, &keyless_account("test"), Timeouts::default(), true);
        assert!(matches!(result, Err(AgentError::Config { .. })));
    }

    fn opencode_go_provider_data(env_key: &str) -> ProviderData {
        ProviderData {
            slug: OPENCODE_GO.into(),
            display_name: "Opencode Go".into(),
            env_keys: vec![env_key.into()],
            base_url: Some("https://opencode.ai/zen/go/v1".into()),
            npm: "@ai-sdk/openai-compatible".into(),
            api_format: EndpointType::ChatCompletions,
            models: HashMap::from([
                (
                    PAID_MODEL.into(),
                    CatalogMeta {
                        context: 128_000,
                        context_excludes_output: false,
                        output: MODEL_OUTPUT,
                        input_price: 1.0,
                        output_price: 2.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        supports_thinking: false,
                        supports_vision: false,
                        reasoning_options: ReasoningOptions::default(),
                        pricing_tiers: Vec::new(),
                    },
                ),
                (
                    FREE_MODEL.into(),
                    CatalogMeta {
                        context: 128_000,
                        context_excludes_output: false,
                        output: MODEL_OUTPUT,
                        input_price: 0.0,
                        output_price: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        supports_thinking: false,
                        supports_vision: false,
                        reasoning_options: ReasoningOptions::default(),
                        pricing_tiers: Vec::new(),
                    },
                ),
            ]),
        }
    }

    fn catalog_model(id: &str) -> Model {
        let data = opencode_go_provider_data(UNSET_KEY_ENV);
        let meta = &data.models[id];
        Model {
            model_revision: crate::model_data::current().revision,
            manifest: None,
            entry: None,
            id: id.into(),
            provider: Arc::from(OPENCODE_GO),
            family: ModelFamily::Generic,
            billing: crate::model::Billing::default(),
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            supports_cache_breakpoints_override: None,
            supports_pdf: false,
            pricing: meta.pricing(),
            dated_rates: None,
            discovered_free: false,
            catalog_free: super::is_free_model(meta),
            max_output_tokens: Some(meta.output),
            context_window: meta.context,
            window_excludes_output: meta.context_excludes_output,
            reasoning_options: meta.reasoning_options.clone(),
            thinking_fields: None,
        }
    }

    /// The dry run refuses exactly where the send does.
    #[test]
    fn gated_free_fallback_hides_models_and_refuses_streaming() {
        let data = opencode_go_provider_data(UNSET_KEY_ENV);
        let provider = super::CatalogProvider::new(
            data,
            &keyless_account(OPENCODE_GO),
            Timeouts::default(),
            false,
        )
        .unwrap();
        assert!(smol::block_on(provider.list_models()).unwrap().is_empty());

        let model = catalog_model(FREE_MODEL);
        let (tx, _rx) = flume::unbounded();
        let sent = smol::block_on(provider.stream_message(
            &model,
            &[],
            "",
            &json!([]),
            &tx,
            RequestOptions::default(),
            None,
        ));
        let dry_run = provider.wire_request(
            &model,
            &[],
            "",
            &json!([]),
            &RequestOptions::default(),
            None,
        );
        assert!(matches!(
            sent,
            Err(AgentError::Config { message }) if message.contains(FREE_MODELS_OPT_IN)
        ));
        assert!(matches!(
            dry_run,
            Err(AgentError::Config { message }) if message.contains(FREE_MODELS_OPT_IN)
        ));
    }

    /// Zen and the per-slug providers share one builder, so a single provider
    /// pins both protocols: where each posts and where its system prompt goes.
    #[test_case(
        EndpointType::ChatCompletions,
        "https://opencode.ai/zen/go/v1/chat/completions",
        "/messages/0/content"
        ; "chat_completions"
    )]
    #[test_case(
        EndpointType::Messages,
        "https://opencode.ai/zen/go/v1/messages",
        "/system/0/text"
        ; "messages"
    )]
    fn wire_request_speaks_the_catalog_format(
        api_format: EndpointType,
        url: &str,
        system_at: &str,
    ) {
        let data = ProviderData {
            api_format,
            ..opencode_go_provider_data(UNSET_KEY_ENV)
        };
        let provider = super::CatalogProvider::new(
            data,
            &api_key_account(OPENCODE_GO, ACCOUNT, CATALOG_CREDENTIAL),
            Timeouts::default(),
            false,
        )
        .unwrap();

        let wire = provider
            .wire_request(
                &catalog_model(PAID_MODEL),
                &[],
                SYSTEM_PROMPT,
                &json!([]),
                &RequestOptions::default(),
                None,
            )
            .unwrap();

        assert_eq!(wire.url, url);
        assert!(
            !wire.url.contains(CATALOG_CREDENTIAL),
            "{CREDENTIAL_IN_URL}"
        );
        assert_eq!(wire.body.pointer(system_at), Some(&json!(SYSTEM_PROMPT)));
        assert_eq!(wire.body["model"], PAID_MODEL);
        assert_eq!(wire.body["max_tokens"], MODEL_OUTPUT);
    }

    #[test]
    fn catalog_provider_list_models_free_fallback_hides_paid_models() {
        let data = opencode_go_provider_data(UNSET_KEY_ENV);
        let provider = super::CatalogProvider::new(
            data,
            &keyless_account(OPENCODE_GO),
            Timeouts::default(),
            true,
        )
        .unwrap();
        let models = smol::block_on(provider.list_models()).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["free-model"]);
    }

    #[test]
    fn catalog_provider_list_models_with_key_shows_all() {
        let data = opencode_go_provider_data(UNSET_KEY_ENV);
        let provider = super::CatalogProvider::new(
            data,
            &api_key_account(OPENCODE_GO, ACCOUNT, CATALOG_CREDENTIAL),
            Timeouts::default(),
            false,
        )
        .unwrap();
        let models = smol::block_on(provider.list_models()).unwrap();
        let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["free-model", "paid-model"]);
    }

    const ONE_BAD_MODEL: &str =
        "one unreadable model must cost that model, not the provider it sits in";

    #[test]
    fn unreadable_model_is_dropped_without_losing_its_provider() {
        let provider: super::schema::CatalogProvider = serde_json::from_str(
            r#"{
                "name": "Vendor",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://vendor.example/v1",
                "models": {
                    "good": {"limit": {"context": 200000, "output": 64000}},
                    "bad": {"limit": {"context": "not a number"}}
                }
            }"#,
        )
        .expect(ONE_BAD_MODEL);

        assert!(provider.models.contains_key("good"), "{ONE_BAD_MODEL}");
        assert!(!provider.models.contains_key("bad"), "{ONE_BAD_MODEL}");
    }

    fn temp_state_dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, state_dir)
    }

    #[test]
    fn catalog_format_messages_for_anthropic() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/anthropic"),
            EndpointType::Messages
        );
    }

    #[test]
    fn catalog_format_chat_for_openai_compat() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/openai-compatible"),
            EndpointType::ChatCompletions
        );
    }

    #[test]
    fn catalog_provider_roundtrip_json() {
        let provider = CatalogProvider {
            name: "Test Provider".into(),
            env: vec!["TEST_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://test.api/v1".into()),
            models: HashMap::from([(
                "test-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.5),
                        output: Some(1.5),
                        cache_read: Some(0.1),
                        cache_write: Some(0.2),
                        ..Default::default()
                    }),
                    provider: None,
                    ..Default::default()
                },
            )]),
        };

        let json = serde_json::to_string_pretty(&provider).unwrap();
        let deserialized: CatalogProvider = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "Test Provider");
        assert_eq!(deserialized.npm, "@ai-sdk/openai-compatible");
        assert!(deserialized.models.contains_key("test-model"));
        let model = &deserialized.models["test-model"];
        let cost = model.cost.as_ref().unwrap();
        assert_eq!(cost.input, Some(0.5));
        assert_eq!(cost.output, Some(1.5));
    }

    #[test]
    fn catalog_index_roundtrip_json() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "test-provider".into(),
            CatalogProvider {
                name: "Test".into(),
                env: vec![],
                npm: "@ai-sdk/openai".into(),
                api: Some("https://test.api/v1".into()),
                models: HashMap::from([(
                    "test-model".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                        ..Default::default()
                    },
                )]),
            },
        );

        let json = serde_json::to_string_pretty(&providers).unwrap();
        let deserialized: CatalogIndex = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.len(), 1);
        assert!(deserialized.contains_key("test-provider"));
    }

    #[test]
    fn catalog_provider_missing_optional_fields() {
        let json = r#"{
            "name": "Minimal",
            "npm": "@ai-sdk/openai",
            "models": {}
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        assert_eq!(provider.name, "Minimal");
        assert!(provider.env.is_empty());
        assert!(provider.api.is_none());
        assert!(provider.models.is_empty());
    }

    #[test]
    fn catalog_model_missing_cost_and_provider() {
        let json = r#"{
            "name": "Test",
            "npm": "@ai-sdk/openai",
            "api": "https://test.api/v1",
            "models": {
                "m1": { "limit": {"context": 64000} }
            }
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        let model = &provider.models["m1"];
        assert_eq!(model.limit.as_ref().unwrap().context, Some(64000));
        assert!(model.cost.is_none());
        assert!(model.provider.is_none());
    }

    const PROMPT_BUDGET_IS_INPUT: &str =
        "a catalog model that distinguishes input from context must budget on input";
    const TIERS_SURVIVE_THE_VIEW: &str =
        "tiered rates must reach the model, or a long prompt bills at the base rate";

    /// The catalog lists both a total window and, for a sixth of the catalog,
    /// the prompt budget inside it. Only the latter is what a conversation is
    /// measured against.
    #[test_case(
        r#"{"limit": {"context": 1050000, "input": 922000, "output": 128000}}"#, 922_000
        ; "input wins where the catalog distinguishes it"
    )]
    #[test_case(
        r#"{"limit": {"context": 200000, "output": 64000}}"#, 200_000
        ; "context is the budget where no input is published"
    )]
    #[test_case(r#"{}"#, crate::providers::catalog::FALLBACK_CATALOG_CONTEXT ; "no limits at all falls back")]
    fn catalog_meta_budgets_the_prompt_not_the_total(json: &str, expected: u32) {
        let model: super::schema::CatalogModel = serde_json::from_str(json).unwrap();
        assert_eq!(
            CatalogMeta::from_model(&model).context,
            expected,
            "{PROMPT_BUDGET_IS_INPUT}"
        );
    }

    const OUTPUT_CARVED_FROM_A_BUDGET: &str = "a window taken from limit.input already excludes the output allowance, so reserving \
         room for a reply inside it holds back twice";

    #[test_case(
        r#"{"limit": {"context": 1050000, "input": 922000, "output": 128000}}"#, true
        ; "a published input budget grants output on top"
    )]
    #[test_case(
        r#"{"limit": {"context": 200000, "output": 64000}}"#, false
        ; "a total window carves the output out of itself"
    )]
    #[test_case(r#"{}"#, false ; "an unpublished window is a total")]
    fn catalog_meta_says_whether_its_window_excludes_output(json: &str, expected: bool) {
        let model: super::schema::CatalogModel = serde_json::from_str(json).unwrap();
        let meta = CatalogMeta::from_model(&model);
        assert_eq!(
            meta.context_excludes_output, expected,
            "{OUTPUT_CARVED_FROM_A_BUDGET}"
        );
        assert_eq!(
            meta.model_info("listed").window_excludes_output,
            expected,
            "{OUTPUT_CARVED_FROM_A_BUDGET}"
        );
    }

    #[test]
    fn catalog_meta_view_carries_pricing_tiers() {
        let model: super::schema::CatalogModel = serde_json::from_str(
            r#"{"cost": {"input": 10.0, "output": 50.0,
                 "tiers": [{"input": 20.0, "output": 75.0, "tier": {"size": 272000}}]}}"#,
        )
        .unwrap();

        let view = super::CatalogMetaView::from(&CatalogMeta::from_model(&model));
        let tier = view.pricing_tiers.first().expect(TIERS_SURVIVE_THE_VIEW);

        assert_eq!(tier.above, 272_000, "{TIERS_SURVIVE_THE_VIEW}");
        assert_eq!(tier.input, 20.0, "{TIERS_SURVIVE_THE_VIEW}");
    }

    const OMITTED_RATE_IS_NOT_FREE: &str =
        "a rate a tier leaves out must bill at the base rate, not at zero";

    #[test]
    fn a_tier_that_omits_a_rate_keeps_the_base_rate() {
        let model: super::schema::CatalogModel = serde_json::from_str(
            r#"{"cost": {"input": 10.0, "output": 50.0, "cache_read": 1.0, "cache_write": 12.5,
                 "tiers": [{"input": 20.0, "output": 75.0, "tier": {"size": 272000}}]}}"#,
        )
        .unwrap();

        let meta = CatalogMeta::from_model(&model);
        let tier = meta.pricing_tiers.first().expect(TIERS_SURVIVE_THE_VIEW);

        assert_eq!(
            (tier.cache_read, tier.cache_write),
            (meta.cache_read, meta.cache_write),
            "{OMITTED_RATE_IS_NOT_FREE}"
        );
        assert_eq!(tier.input, 20.0, "{TIERS_SURVIVE_THE_VIEW}");
    }

    #[test]
    fn catalog_provider_resolve_api_key_from_env() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["CAUDRA_TEST_UNUSED_VAR_1"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        // No env var set — returns None (no OPENCODE_API_KEY in env)
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_resolve_api_key_anthropic_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["ANTHROPIC_SECRET_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        // ANTHROPIC_SECRET_KEY is not set.
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_build_auth_no_key_returns_none() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec![],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        // No env vars and no OPENCODE_API_KEY fallback — no auth
        assert!(matches!(
            provider_data.build_auth(&state_dir),
            Authentication::NoAuth
        ));
    }

    #[test]
    fn catalog_provider_build_auth_public_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["OPENCODE_API_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "opencode".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::OpenCodeFreeKey(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer public");
            }
            _ => panic!("expected OpenCodeFreeKey"),
        }
    }

    #[test]
    fn catalog_provider_build_auth_key_based() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("CAUDRA_TEST_AUTH_KEY", "sk-real-key") };
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["CAUDRA_TEST_AUTH_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer sk-real-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("CAUDRA_TEST_AUTH_KEY") };
    }

    #[test]
    fn catalog_provider_build_auth_x_api_key() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("CAUDRA_TEST_ANTHROPIC_KEY", "sk-ant-key") };
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["CAUDRA_TEST_ANTHROPIC_KEY"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "x-api-key");
                assert_eq!(resolved.headers[0].1, "sk-ant-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("CAUDRA_TEST_ANTHROPIC_KEY") };
    }

    #[test]
    fn catalog_to_data_filters_nonfree_without_key() {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(2.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["CAUDRA_TEST_VENDOR_KEY_60924".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers);
        // No key filter — all models pass regardless of key status
        let vendor = result.providers.get("some-vendor").unwrap();
        assert_eq!(vendor.models.len(), 2, "all models included");
    }

    #[test]
    fn catalog_to_data_opencode_free_models_without_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers);
        // No key filter — all models pass regardless of key status
        let opencode = result.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2, "all models included");
        // Public fallback auth registered
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
    }

    #[test_case("free-model", true; "free_opencode_model_is_free")]
    #[test_case("paid-output-model", false; "free_input_paid_output_is_not_free")]
    fn model_is_free_uses_catalog_definition(model_id: &str, expected: bool) {
        let models = HashMap::from([
            (
                "free-model".into(),
                CatalogModel {
                    limit: None,
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(0.0),
                        cache_read: None,
                        cache_write: None,
                        ..Default::default()
                    }),
                    provider: None,
                    ..Default::default()
                },
            ),
            (
                "paid-output-model".into(),
                CatalogModel {
                    limit: None,
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(25.0),
                        cache_read: None,
                        cache_write: None,
                        ..Default::default()
                    }),
                    provider: None,
                    ..Default::default()
                },
            ),
        ]);
        let index: CatalogIndex = HashMap::from([(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        )]);
        let snapshot = caudra_models::Snapshot::stamp(
            caudra_models::Payload {
                catalog: serde_json::from_value(serde_json::to_value(index).unwrap()).unwrap(),
                ..Default::default()
            },
            None,
            NOW.parse().unwrap(),
        );
        crate::model_data::install(snapshot);

        let model = super::Model::from_spec(&format!("opencode/{model_id}")).unwrap();
        assert_eq!(model.is_free(), expected);
    }

    #[test]
    fn catalog_to_data_opencode_all_models_with_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["CAUDRA_TEST_OPENCODE_ALL_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_OPENCODE_ALL_81274", "real-key") };
        let result = CatalogData::from_index(providers);

        // With key set, has_api_key is true, so all models pass
        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::KeyBased(_)
        ));
        unsafe { std::env::remove_var("CAUDRA_TEST_OPENCODE_ALL_81274") };
    }

    fn opencode_catalog_with_free_and_paid(_env_var: &str) -> CatalogIndex {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );
        providers
    }

    #[test]
    fn catalog_to_data_opencode_hides_free_models_when_disabled() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = opencode_catalog_with_free_and_paid("unused");
        let result = CatalogData::from_index(index);

        // All models are in entries (filtering happens in all_models)
        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        // Provider ID is "opencode" so slug becomes "opencode" -> OpenCodeFreeKey fallback
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
        // all_models with OpenCodeFreeKey only shows free models; with enable_free_models=false, no models shown
        assert!(opencode.available_models(&state_dir, false).is_empty());
    }

    #[test]
    fn catalog_to_data_opencode_no_models_without_key_when_disabled() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = opencode_catalog_with_free_and_paid("unused");
        let result = CatalogData::from_index(index);

        // All models are in entries (filtering happens in all_models)
        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        // Provider ID is "opencode" so slug becomes "opencode" -> OpenCodeFreeKey fallback
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
        // all_models filters both free (enable_free_models=false) and paid (OpenCodeFreeKey only shows free)
        assert!(opencode.available_models(&state_dir, false).is_empty());
    }

    #[test]
    fn catalog_to_data_all_models_with_key() {
        let mut models = HashMap::new();
        models.insert(
            "cheap".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "freebie".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["CAUDRA_TEST_VENDOR_KEY_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_VENDOR_KEY_81274", "test-key") };
        let result = CatalogData::from_index(providers);
        unsafe { std::env::remove_var("CAUDRA_TEST_VENDOR_KEY_81274") };

        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("cheap")
        );
        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("freebie")
        );
    }

    #[test]
    fn catalog_to_data_skips_providers_without_api_url() {
        let mut providers = HashMap::new();
        providers.insert(
            "no-api".into(),
            CatalogProvider {
                name: "No API".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: None,
                models: HashMap::new(),
            },
        );

        let result = CatalogData::from_index(providers);
        assert!(result.providers.is_empty());
    }

    #[test]
    fn catalog_to_data_handles_model_id_collisions() {
        let mut models: HashMap<String, CatalogModel> = HashMap::new();
        models.insert(
            "shared-model".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(64_000),
                    input: Some(64_000),
                    output: Some(8_000),
                }),
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();

        // Provider "opencode" has "shared-model"
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: models.clone(),
            },
        );

        // Provider "other-vendor" also has "shared-model"
        providers.insert(
            "other-vendor".into(),
            CatalogProvider {
                name: "Other".into(),
                env: vec!["CAUDRA_TEST_OTHER_KEY_COLLISION".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://other.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_OTHER_KEY_COLLISION", "key") };
        let result = CatalogData::from_index(providers);
        unsafe { std::env::remove_var("CAUDRA_TEST_OTHER_KEY_COLLISION") };

        // Both providers' entries are preserved
        assert!(
            result
                .providers
                .get("opencode")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert!(
            result
                .providers
                .get("other-vendor")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert_eq!(result.providers.len(), 2);

        // lookup prefers the "opencode" provider
        // lookup expects "provider/model_id" format
        let (_meta, provider_data) = result.lookup("opencode", "shared-model").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_opencode_own_models() {
        let mut models = HashMap::new();
        models.insert(
            "opus".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers);
        let (_meta, provider_data) = data.lookup("opencode", "opus").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_model_id_with_slashes() {
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["CAUDRA_TEST_NVIDIA_KEY_LOOKUP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_NVIDIA_KEY_LOOKUP", "key") };
        let data = CatalogData::from_index(providers);
        unsafe { std::env::remove_var("CAUDRA_TEST_NVIDIA_KEY_LOOKUP") };

        // Entry is stored as ("nvidia", "openai/gpt-oss-120b")
        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_spec_is_sub_provider_plus_model_id() {
        // Simulates the stream_message pattern:
        // lookup key = "{sub_provider}/{model.id}"
        // e.g. "nvidia/openai/gpt-oss-120b"
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["CAUDRA_TEST_NVIDIA_DIRECT".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_NVIDIA_DIRECT", "key") };
        let data = CatalogData::from_index(providers);
        unsafe { std::env::remove_var("CAUDRA_TEST_NVIDIA_DIRECT") };

        // The lookup key constructed by stream_message:
        // format!("{}/{}", sub_provider, model.id)
        // = "nvidia/openai/gpt-oss-120b"
        let _key = format!("{}/{}", "nvidia", "openai/gpt-oss-120b");
        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_nested_model_id_uses_sub_provider_key() {
        let mut models = HashMap::new();
        models.insert(
            "deepseek-ai/DeepSeek-R1".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "fireworks".into(),
            CatalogProvider {
                name: "Fireworks".into(),
                env: vec!["CAUDRA_TEST_FIREWORKS_DEEP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://fireworks.ai/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CAUDRA_TEST_FIREWORKS_DEEP", "key") };
        let data = CatalogData::from_index(providers);
        unsafe { std::env::remove_var("CAUDRA_TEST_FIREWORKS_DEEP") };

        // stream_message constructs key as "{sub_provider}/{model.id}"
        // = "fireworks/deepseek-ai/DeepSeek-R1"
        let _key = format!("{}/{}", "fireworks", "deepseek-ai/DeepSeek-R1");
        let (_meta, provider_data) = data.lookup("fireworks", "deepseek-ai/DeepSeek-R1").unwrap();
        assert_eq!(provider_data.slug, "fireworks");
    }

    #[test]
    fn catalog_all_models_filters_keyless_providers() {
        let (_tmp, state_dir) = temp_state_dir();
        save_account(
            &state_dir,
            &AccountId::new("keyed", AccountName::new("default").unwrap()),
            &StoredAccount {
                secret: StoredSecret::ApiKey(ProviderCredentials {
                    api_key: "sk-abc123".into(),
                    host: None,
                }),
                identity: AccountIdentity::default(),
                created_at: 0,
            },
        )
        .unwrap();

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "keyed".into(),
            CatalogProvider {
                name: "Keyed".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyed.api/v1".into()),
                models: HashMap::from([(
                    "m1".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                        ..Default::default()
                    },
                )]),
            },
        );
        providers.insert(
            "keyless".into(),
            CatalogProvider {
                name: "Keyless".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyless.api/v1".into()),
                models: HashMap::from([(
                    "m2".into(),
                    CatalogModel {
                        limit: None,
                        cost: Some(CatalogCost {
                            input: Some(5.0),
                            output: Some(10.0),
                            cache_read: None,
                            cache_write: None,
                            ..Default::default()
                        }),
                        provider: None,
                        ..Default::default()
                    },
                )]),
            },
        );

        let data = CatalogData::from_index(providers);

        // Entries include both providers
        assert_eq!(data.providers.len(), 2);

        // all_models should only include the keyed provider's model
        let models: Vec<_> = data
            .providers
            .values()
            .flat_map(|provider| provider.available_models(&state_dir, true))
            .collect();
        assert_eq!(
            models.len(),
            1,
            "should only include models from providers with keys"
        );
        assert_eq!(models[0].id, "m1");
    }

    #[test]
    fn catalog_all_models_public_fallback_shows_only_free() {
        let (_tmp, state_dir) = temp_state_dir();
        // Provider with OPENCODE_API_KEY in env but no key set gets "public" fallback.
        // Only free (zero-cost) models should appear in all_models.
        let mut models = HashMap::new();
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(3.0),
                    cache_read: None,
                    cache_write: None,
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        // No OPENCODE_API_KEY set in env — falls back to "public"
        let data = CatalogData::from_index(providers);

        // Both models are in providers
        let opencode = data.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2);
        // But all_models only returns the free one
        let result = opencode.available_models(&state_dir, true);
        assert_eq!(
            result.len(),
            1,
            "public fallback should only show free models"
        );
        assert_eq!(result[0].id, "free-model");
        assert_eq!(result[0].pricing.as_ref().unwrap().input, 0.0);
    }

    #[test]
    fn catalog_lookup_finds_model_by_opencode_key() {
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.1-codex-mini".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(128_000),
                    input: None,
                    output: Some(16_384),
                }),
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(5.0),
                    cache_read: Some(0.1),
                    cache_write: Some(0.2),
                    ..Default::default()
                }),
                provider: None,
                ..Default::default()
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "OpenCode Zen".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers);

        let (meta, provider_data) = data.lookup("opencode", "gpt-5.1-codex-mini").unwrap();
        assert_eq!(provider_data.slug, "opencode");
        assert_eq!(meta.context, 128_000);
        assert_eq!(meta.output, 16_384);
    }

    #[test]
    fn catalog_lookup_rejects_unknown_provider_key() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "OpenCode Zen".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: HashMap::from([("gpt-5.1-codex-mini".into(), CatalogModel::default())]),
            },
        );

        let data = CatalogData::from_index(providers);

        assert!(
            data.lookup("unknown-provider", "gpt-5.1-codex-mini")
                .is_err()
        );
    }

    #[test]
    fn catalog_lookup_finds_model_by_opencode_go_key() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "opencode-go".into(),
            CatalogProvider {
                name: "OpenCode Go".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/go/v1".into()),
                models: HashMap::from([("fast-model".into(), CatalogModel::default())]),
            },
        );

        let data = CatalogData::from_index(providers);

        let (_meta, provider_data) = data.lookup("opencode-go", "fast-model").unwrap();
        assert_eq!(provider_data.slug, "opencode-go");
    }
}

pub(crate) mod schema {
    //! Serde types for the model list's catalog section, in the models.dev shape.

    use std::collections::HashMap;

    pub use caudra_models::{CatalogCost, CatalogLimits, CatalogModalities, CatalogShape};
    use caudra_storage::thinking::ReasoningOptions;
    use serde::{Deserialize, Serialize};

    pub type CatalogIndex = HashMap<String, CatalogProvider>;

    #[derive(Deserialize, Serialize)]
    pub struct CatalogProvider {
        pub name: String,
        #[serde(default)]
        pub env: Vec<String>,
        pub npm: String,
        pub api: Option<String>,
        #[serde(deserialize_with = "deserialize_models")]
        pub models: HashMap<String, CatalogModel>,
    }

    /// Drops the models caudra cannot read instead of failing the whole catalog
    /// with them. The catalog describes thousands of models; one of them
    /// carrying a shape this schema has not met yet must cost that one
    /// model, not every provider caudra can route to.
    fn deserialize_models<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<String, CatalogModel>, D::Error> {
        let raw = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
        Ok(raw
            .into_iter()
            .filter_map(
                |(id, value)| match serde_json::from_value::<CatalogModel>(value) {
                    Ok(model) => Some((id, model)),
                    Err(error) => {
                        tracing::debug!(model = %id, %error, "skipping unreadable catalog model");
                        None
                    }
                },
            )
            .collect())
    }

    #[derive(Default, Deserialize, Serialize, Clone)]
    pub struct CatalogModel {
        pub limit: Option<CatalogLimits>,
        #[serde(default)]
        pub cost: Option<CatalogCost>,
        #[serde(default)]
        pub provider: Option<CatalogShape>,
        #[serde(default)]
        pub attachment: bool,
        #[serde(default)]
        pub reasoning: bool,
        #[serde(default)]
        pub modalities: Option<CatalogModalities>,
        /// Per-model reasoning controls. Absent for models the catalog has not
        /// classified, which is not the same as a model that cannot reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_options: Option<ReasoningOptions>,
    }
}
