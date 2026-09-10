use std::sync::{Arc, Mutex};

use caudra_storage::id::SessionRef;
use caudra_storage::thinking::EFFORT_NONE;
use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Message, ProviderEvent, ReasoningOption, ReasoningOptions, RequestOptions,
    StreamResponse,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const REFERER: &str = "https://caudra.ai";
const APP_TITLE: &str = "caudra";
const PER_MILLION: f64 = 1_000_000.0;

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "openrouter",
    api_key_env: "OPENROUTER_API_KEY",
    base_url: "https://openrouter.ai/api/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "OpenRouter",
};

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "openrouter",
    display_name: "OpenRouter",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://openrouter.ai/api/v1",
    default_api_key_env: "OPENROUTER_API_KEY",
    default_model: "openrouter/openai/gpt-5.5",
    plans: None,
    login_url: Some("https://openrouter.ai/keys"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct OpenRouter {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl OpenRouter {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve(CONFIG.slug, CONFIG.api_key_env)?;
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
}

/// OpenRouter reports reasoning per model, in the three states its API
/// documents. Translated into what the model accepts, so it resolves through
/// the same path as every other provider:
/// 1. mandatory - always on, so there is no way to spell off.
/// 2. default_enabled - on unless told otherwise, and "none" is how you tell it.
/// 3. default off - a plain toggle; omitting the field keeps it off.
fn reasoning_options(reasoning: &serde_json::Map<String, Value>) -> ReasoningOptions {
    let mandatory = reasoning.get("mandatory").and_then(Value::as_bool) == Some(true);
    let default_enabled = reasoning.get("default_enabled").and_then(Value::as_bool) == Some(true);

    let mut values: Vec<String> = reasoning
        .get("supported_efforts")
        .and_then(Value::as_array)
        .map(|declared| {
            declared
                .iter()
                .filter_map(|value| Some(value.as_str()?.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut options = Vec::new();
    if !mandatory && !default_enabled {
        options.push(ReasoningOption::Toggle);
    }
    if !mandatory && default_enabled && !values.iter().any(|value| value == EFFORT_NONE) {
        values.insert(0, EFFORT_NONE.to_string());
    }
    if !values.is_empty() {
        options.push(ReasoningOption::Effort { values });
    }
    ReasoningOptions::new(options)
}

fn parse_model(m: &Value) -> Option<ModelInfo> {
    // Filter: only text input/output models
    let architecture = m["architecture"].as_object()?;
    let input_modalities = architecture["input_modalities"].as_array()?;
    let output_modalities = architecture["output_modalities"].as_array()?;

    let has_text_input = input_modalities.iter().any(|m| m.as_str() == Some("text"));
    let has_text_output = output_modalities.iter().any(|m| m.as_str() == Some("text"));
    if !has_text_input || !has_text_output {
        return None;
    }

    let supports_vision = input_modalities.iter().any(|m| m.as_str() == Some("image"));

    // Parse with OpenRouter-specific pricing field names. OpenRouter reports
    // per-token prices; scale to $/M as `ModelPricing` expects. A missing or
    // unparsable price stays `None` so it never reads as free.
    let id = m["id"].as_str()?;
    let context_window = m["context_length"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok());
    let per_token =
        |p: &Value| -> Option<f64> { Some(p.as_str()?.parse::<f64>().ok()? * PER_MILLION) };
    let pricing = m["pricing"].as_object().and_then(|p| {
        Some(ModelPricing {
            input: per_token(p.get("prompt")?)?,
            output: per_token(p.get("completion")?)?,
            cache_write: p
                .get("input_cache_write")
                .and_then(per_token)
                .unwrap_or(0.0),
            cache_read: p.get("input_cache_read").and_then(per_token).unwrap_or(0.0),
            fast: None,
            tiers: Vec::new(),
        })
    });

    let reasoning = m.get("reasoning").and_then(Value::as_object);

    let supports_thinking = reasoning.is_some()
        || m.get("supported_parameters")
            .and_then(|v| v.as_array())
            .is_some_and(|v| v.iter().any(|v| v.as_str() == Some("reasoning")));

    Some(ModelInfo {
        id: id.to_string(),
        context_window,
        max_output_tokens: None,
        pricing,
        supports_thinking: Some(supports_thinking),
        supports_vision: Some(supports_vision),
        reasoning_options: reasoning.map(reasoning_options),
        provider_info: None,
    })
}

impl Provider for OpenRouter {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            body["cache_control"] = json!({"type": "ephemeral"});

            if model.supports_thinking()
                && let Some(effort) = opts.thinking.effort_str(model)
            {
                body["reasoning"] = json!({"effort": effort});
            }

            if let Some(sid) = session_id {
                body["session_id"] = json!(sid.to_string());
            }

            let extra_headers = [("HTTP-Referer", REFERER), ("X-OpenRouter-Title", APP_TITLE)];
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.fetch_and_parse_models(&auth, parse_model).await
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

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::ThinkingConfig;

    const UNKNOWN_PRICE_STAYS_UNKNOWN: &str = "a price we cannot read must not become a zero price";

    fn kimi_k3_json() -> Value {
        json!({
            "id": "moonshotai/kimi-k3",
            "context_length": 1_048_576,
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
            },
            "pricing": {
                "prompt": "0.000003",
                "completion": "0.000015",
                "input_cache_read": "0.0000003",
            },
            "supported_parameters": ["reasoning"],
        })
    }

    #[test]
    fn parse_model_scales_pricing_to_per_million() {
        let info = parse_model(&kimi_k3_json()).expect("model should parse");

        assert_eq!(info.id, "moonshotai/kimi-k3");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.supports_vision, Some(true));
        assert_eq!(info.supports_thinking, Some(true));
        let pricing = info.pricing.expect("pricing should be parsed");
        assert_eq!(pricing.input, 3.0);
        assert_eq!(pricing.output, 15.0);
        assert_eq!(pricing.cache_read, 0.3);
        assert_eq!(pricing.cache_write, 0.0);
    }

    #[test]
    fn parse_model_scales_cache_write() {
        let mut m = kimi_k3_json();
        m["pricing"]["input_cache_write"] = json!("0.00000375");

        let pricing = parse_model(&m)
            .expect("model should parse")
            .pricing
            .expect("pricing should be parsed");
        assert_eq!(pricing.cache_write, 3.75);
    }

    /// A price we cannot read used to collapse to an all-zero `ModelPricing`,
    /// which downstream reads as "free". Unknown has to stay unknown.
    #[test_case(json!(null)                                       ; "no_pricing_object")]
    #[test_case(json!({"prompt": "0.000003"})                     ; "no_completion")]
    #[test_case(json!({"prompt": "n/a", "completion": "0.000015"}) ; "unparsable_prompt")]
    fn parse_model_keeps_unusable_pricing_unknown(pricing: Value) {
        let mut m = kimi_k3_json();
        m["pricing"] = pricing;

        let info = parse_model(&m).expect("model should parse");
        assert!(info.pricing.is_none(), "{UNKNOWN_PRICE_STAYS_UNKNOWN}");
    }

    #[test]
    fn parse_model_keeps_declared_efforts_in_order() {
        let mut m = kimi_k3_json();
        m["reasoning"] = json!({
            "mandatory": false,
            "default_enabled": true,
            "supported_efforts": ["high", "bogus", "low", "none"],
        });

        let info = parse_model(&m).expect("model should parse");
        let options = info
            .reasoning_options
            .expect("reasoning should be declared");
        // Declaration order is the ladder, and a spelling caudra does not know is
        // still the provider's to declare.
        assert_eq!(options.efforts(), ["high", "bogus", "low", "none"]);
        assert!(!options.has_toggle());
    }

    fn openrouter_model(reasoning: Value) -> Model {
        let info = parse_model(&json!({
            "id": "vendor/test-model",
            "context_length": 200_000,
            "architecture": {
                "input_modalities": ["text"],
                "output_modalities": ["text"],
            },
            "reasoning": reasoning,
        }))
        .expect("model should parse");
        let mut model = Model::from_spec("openrouter/vendor/test-model").unwrap();
        model.reasoning_options = info.reasoning_options.unwrap_or_default();
        model
    }

    #[test_case(json!(["high", "xhigh"]), ThinkingConfig::Effort("xhigh".into()), Some("xhigh") ; "declared_level_passes_through")]
    #[test_case(json!(["high", "xhigh"]), ThinkingConfig::Effort("max".into()),   Some("xhigh") ; "undeclared_level_snaps_down")]
    #[test_case(json!(["minimal", "low"]), ThinkingConfig::Adaptive,              None          ; "adaptive_leaves_the_choice_to_the_model")]
    #[test_case(json!([]), ThinkingConfig::Effort("xhigh".into()),                Some("xhigh") ; "undeclared_model_sends_what_was_asked")]
    fn effort_resolves_against_declared_levels(
        efforts: Value,
        config: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let model = openrouter_model(json!({"supported_efforts": efforts}));
        assert_eq!(config.effort_str(&model).as_deref(), expected);
    }

    #[test_case(false, false, None         ; "default_off_sends_nothing")]
    #[test_case(true,  false, Some("none") ; "default_enabled_disables_with_none")]
    #[test_case(true,  true,  None         ; "mandatory_cannot_be_disabled")]
    fn off_resolves_per_reasoning_flags(
        default_enabled: bool,
        mandatory: bool,
        expected: Option<&str>,
    ) {
        let model = openrouter_model(json!({
            "default_enabled": default_enabled,
            "mandatory": mandatory,
        }));
        assert_eq!(ThinkingConfig::Off.effort_str(&model).as_deref(), expected);
    }

    #[test_case(json!(["image"]), json!(["image"]); "image_only")]
    #[test_case(json!(["image"]), json!(["text"]); "image_input_only")]
    #[test_case(json!(["text"]), json!(["image"]); "image_output_only")]
    fn parse_model_skips_non_text_models(input: Value, output: Value) {
        let mut m = kimi_k3_json();
        m["architecture"]["input_modalities"] = input;
        m["architecture"]["output_modalities"] = output;

        assert!(parse_model(&m).is_none());
    }
}
