use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "synthetic",
    api_key_env: "SYNTHETIC_API_KEY",
    base_url: "https://api.synthetic.new/openai/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: false,
    provider_name: "Synthetic",
};

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "synthetic",
    display_name: "Synthetic",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.synthetic.new/openai/v1",
    default_api_key_env: "SYNTHETIC_API_KEY",
    default_model: "synthetic/hf:moonshotai/Kimi-K2.5",
    plans: None,
    login_url: Some("https://synthetic.new"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["hf:moonshotai/Kimi-K2.5"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.45,
                output: 3.40,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(131072),
            context_window: 200_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["hf:deepseek-ai/DeepSeek-V3.2"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.56,
                output: 1.68,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(131072),
            context_window: 200_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["hf:zai-org/GLM-4.7-Flash"],
            small: true,
            family: ModelFamily::Synthetic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.50,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(131072),
            context_window: 200_000,
            reasoning_options: None,
        },
    ];
    MODELS
}

pub struct Synthetic {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Synthetic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("synthetic", CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(pool.current()))),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    /// What a turn posts, for the send and the dry run alike.
    fn request(
        &self,
        auth: &ResolvedAuth,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        thinking: &ThinkingConfig,
    ) -> WireRequest {
        let mut buf = String::new();
        let system = super::with_prefix(&self.system_prefix, system, &mut buf);
        let mut body = self.compat.build_body(model, messages, system, tools);
        thinking.apply_reasoning_effort(&mut body, model);
        WireRequest::post(self.compat.chat_url(auth), body)
    }
}

impl Provider for Synthetic {
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
            let auth = self.auth.lock().unwrap().clone();
            let wire = self.request(&auth, model, messages, system, tools, &opts.thinking);
            self.compat
                .do_stream(model, &[], &wire, event_tx, &auth)
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
        let auth = self.auth.lock().unwrap().clone();
        Ok(self.request(&auth, model, messages, system, tools, &opts.thinking))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
        })
    }
}
