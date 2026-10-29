use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use crate::accounts::AccountCredential;
use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, StaticReasoningOption};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// Each ladder is the model's `reasoning_parameters.efforts` on Synthetic's
/// `/openai/v1/models`. A ladder without `none` cannot be switched off, so off
/// asks for its lowest level.
const EFFORT_LOW_HIGH_MAX: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "high", "max"])];
const EFFORT_NONE_TO_MAX: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "high", "xhigh", "max",
])];
const EFFORT_NONE_TO_HIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high",
])];
const QWEN_EFFORT: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "medium", "xhigh"])];

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "synthetic",
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
    default_model: "synthetic/hf:moonshotai/Kimi-K3",
    plans: None,
    login_url: Some("https://synthetic.new"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["hf:moonshotai/Kimi-K3"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 0.00,
                cache_read: 0.45,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 524_288,
            reasoning_options: Some(EFFORT_LOW_HIGH_MAX),
        },
        ModelEntry {
            prefixes: &["hf:zai-org/GLM-5.3"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 1.40,
                output: 4.40,
                cache_write: 0.00,
                cache_read: 0.26,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 524_288,
            reasoning_options: Some(EFFORT_LOW_HIGH_MAX),
        },
        ModelEntry {
            prefixes: &["hf:deepseek-ai/DeepSeek-V4.1-Flash-1017"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.60,
                output: 1.20,
                cache_write: 0.00,
                cache_read: 0.03,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 524_288,
            reasoning_options: Some(EFFORT_NONE_TO_MAX),
        },
        ModelEntry {
            prefixes: &["hf:deepseek-ai/DeepSeek-V4.1-Flash"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.60,
                output: 1.20,
                cache_write: 0.00,
                cache_read: 0.03,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 524_288,
            reasoning_options: Some(EFFORT_NONE_TO_MAX),
        },
        ModelEntry {
            prefixes: &["hf:nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.30,
                output: 1.00,
                cache_write: 0.00,
                cache_read: 0.06,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 262_144,
            reasoning_options: Some(EFFORT_NONE_TO_HIGH),
        },
        ModelEntry {
            prefixes: &["hf:openai/gpt-oss-120b"],
            small: false,
            family: ModelFamily::Synthetic,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.10,
                cache_write: 0.00,
                cache_read: 0.02,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 131_072,
            reasoning_options: Some(EFFORT_NONE_TO_HIGH),
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
                cache_read: 0.02,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 196_608,
            reasoning_options: Some(EFFORT_NONE_TO_HIGH),
        },
        ModelEntry {
            prefixes: &["hf:zai-org/GLM-5.3-Flash"],
            small: true,
            family: ModelFamily::Synthetic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.50,
                cache_write: 0.00,
                cache_read: 0.04,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 524_288,
            reasoning_options: Some(EFFORT_LOW_HIGH_MAX),
        },
        ModelEntry {
            prefixes: &["hf:Qwen/Qwen3.8-27B"],
            small: true,
            family: ModelFamily::Synthetic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.45,
                output: 2.20,
                cache_write: 0.00,
                cache_read: 0.09,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: Some(65_536),
            context_window: 262_144,
            reasoning_options: Some(QWEN_EFFORT),
        },
    ];
    MODELS
}

pub struct Synthetic {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

impl Synthetic {
    pub fn new(account: &AccountCredential, timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(account.api_key()?))),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
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
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::model::lookup_entry;

    const SLUG: &str = "synthetic";

    #[test_case("hf:moonshotai/Kimi-K3", 3.00, 0.45, 15.00, 524_288, true ; "kimi_k3")]
    #[test_case("hf:zai-org/GLM-5.3", 1.40, 0.26, 4.40, 524_288, false ; "glm_5_3")]
    #[test_case("hf:zai-org/GLM-5.3-Flash", 0.15, 0.04, 0.50, 524_288, true ; "glm_5_3_flash")]
    #[test_case("hf:zai-org/GLM-4.7-Flash", 0.10, 0.02, 0.50, 196_608, false ; "glm_4_7_flash")]
    #[test_case("hf:deepseek-ai/DeepSeek-V4.1-Flash", 0.60, 0.03, 1.20, 524_288, true ; "deepseek_v4_1_flash")]
    #[test_case("hf:deepseek-ai/DeepSeek-V4.1-Flash-1017", 0.60, 0.03, 1.20, 524_288, true ; "deepseek_v4_1_flash_1017")]
    #[test_case("hf:Qwen/Qwen3.8-27B", 0.45, 0.09, 2.20, 262_144, true ; "qwen3_8_27b")]
    #[test_case("hf:nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4", 0.30, 0.06, 1.00, 262_144, false ; "nemotron_3_super")]
    #[test_case("hf:openai/gpt-oss-120b", 0.10, 0.02, 0.10, 131_072, false ; "gpt_oss_120b")]
    fn served_model_resolves_to_its_own_entry(
        id: &str,
        input: f64,
        cache_read: f64,
        output: f64,
        context_window: u32,
        vision: bool,
    ) {
        let entry = lookup_entry(models(), id).unwrap();
        assert!(entry.prefixes.contains(&id));
        assert_eq!(entry.pricing.input, input);
        assert_eq!(entry.pricing.cache_read, cache_read);
        assert_eq!(entry.pricing.output, output);
        assert_eq!(entry.context_window, context_window);
        assert_eq!(entry.vision, vision);
    }

    #[test_case("hf:moonshotai/Kimi-K3", "low" ; "kimi_k3_cannot_switch_off")]
    #[test_case("hf:deepseek-ai/DeepSeek-V4.1-Flash", "none" ; "deepseek_declares_none")]
    fn off_follows_the_declared_ladder(id: &str, expected: &str) {
        let model = Model::from_spec(&format!("{SLUG}/{id}")).unwrap();
        assert_eq!(
            ThinkingConfig::Off.effort_str(&model).as_deref(),
            Some(expected)
        );
    }
}
