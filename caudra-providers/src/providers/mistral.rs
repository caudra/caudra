use std::sync::{Arc, Mutex};

use flume::Sender;
use jiff::civil;
use serde_json::{Value, json};

use crate::accounts::AccountCredential;
use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ThinkingSupport};
use crate::pricing::PriceChange;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

const LARGE_4: &str = "mistral-large-4";

/// Mistral Large 4 launched on 2026-10-06 at half price "for 2 weeks"
/// (docs.mistral.ai/getting-started/changelog), so list price from 2026-10-20,
/// as <https://docs.mistral.ai/models/mistral-large-4-0> quotes it.
pub(crate) const PRICE_CHANGES: &[PriceChange] = &[PriceChange {
    prefix: LARGE_4,
    from: civil::date(2026, 10, 20),
    pricing: ModelPricing {
        input: 1.36,
        output: 4.18,
        cache_write: 0.00,
        cache_read: 0.14,
        fast: None,
        tiers: ModelPricing::UNTIERED,
    },
}];

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "mistral",
    base_url: "https://api.mistral.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Mistral",
};

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "mistral",
    display_name: "Mistral",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.mistral.ai/v1",
    default_api_key_env: "MISTRAL_API_KEY",
    default_model: "mistral/mistral-medium-latest",
    plans: Some(&[
        (
            "standard",
            caudra_config::providers::ProviderPlan {
                display_name: "Standard",
                base_url: "https://api.mistral.ai/v1",
                default_model: Some("mistral/mistral-medium-latest"),
                login_url: None,
            }
        ),
        (
            "coding",
            caudra_config::providers::ProviderPlan {
                display_name: "Vibe / Coding",
                base_url: "https://api.mistral.ai/v1",
                default_model: Some("mistral/mistral-vibe-cli-latest"),
                login_url: Some("https://console.mistral.ai/codestral/cli"),
            }
        ),
    ]),
    login_url: Some("https://admin.mistral.ai/organization/api-keys"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &[
                "mistral-medium-latest",
                "mistral-medium-3.5",
                "mistral-medium-3-5",
                "mistral-medium-2604",
                "mistral-medium-3",
            ],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.5,
                output: 7.5,
                cache_write: 0.00,
                cache_read: 0.15,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 262_144,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &[LARGE_4],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.68,
                output: 2.09,
                cache_write: 0.00,
                cache_read: 0.07,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 1_000_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["mistral-large-latest", "mistral-large-2512"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.50,
                output: 1.50,
                cache_write: 0.00,
                cache_read: 0.05,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 262_144,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["zai-glm-5-3-flash"],
            small: true,
            family: ModelFamily::Glm,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.50,
                cache_write: 0.00,
                cache_read: 0.03,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 1_000_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["glm-5-3", "zai-glm-5-3", "zai-glm-5", "zai-glm-latest"],
            small: false,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 1.40,
                output: 4.40,
                cache_write: 0.00,
                cache_read: 0.14,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 1_000_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["glm-5-2", "zai-glm-5-2"],
            small: false,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 1.40,
                output: 4.40,
                cache_write: 0.00,
                cache_read: 0.14,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 1_000_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["deepseek-v4-1-flash-1017"],
            small: false,
            family: ModelFamily::Generic,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.30,
                output: 1.20,
                cache_write: 0.00,
                cache_read: 0.007,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 1_000_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["mistral-small-latest", "mistral-small-2603"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.015,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 262_144,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["ministral-14b-latest", "ministral-14b-2512"],
            small: true,
            family: ModelFamily::Generic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.20,
                output: 0.20,
                cache_write: 0.00,
                cache_read: 0.02,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 262_144,
            reasoning_options: None,
        },
    ];
    MODELS
}

pub struct Mistral {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

fn convert_assistant_messages_in_place(messages: &mut Value) {
    if let Some(msgs) = messages.as_array_mut() {
        for msg in msgs {
            if let Some(obj) = msg.as_object_mut()
                && obj.get("role").and_then(Value::as_str) == Some("assistant")
            {
                let Some(reasoning_val) = obj.remove("reasoning_content") else {
                    continue;
                };
                let Some(reasoning_text) = reasoning_val.as_str() else {
                    continue;
                };

                let thinking_block = json!({
                    "type": "thinking",
                    "thinking": [{"type": "text", "text": reasoning_text}]
                });

                if let Some(content) = obj.get_mut("content") {
                    if let Some(content_str) = content.as_str()
                        && !content_str.is_empty()
                    {
                        // Has text content, create array with both
                        let text_content = json!({"type": "text", "text": content_str});
                        *content = json!([thinking_block, text_content]);
                    } else if content.is_string() {
                        // Empty string content, just use thinking
                        *content = json!([thinking_block]);
                    } else if let Some(arr) = content.as_array_mut() {
                        // Already an array, prepend thinking
                        arr.insert(0, thinking_block);
                    } else {
                        *content = json!([thinking_block]);
                    }
                } else {
                    obj.insert("content".to_string(), json!([thinking_block]));
                }
            }
        }
    }
}

impl Mistral {
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
        convert_assistant_messages_in_place(&mut body["messages"]);
        WireRequest::post(self.compat.chat_url(auth), body)
    }
}

impl Provider for Mistral {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let wire = self.request(&auth, model, messages, system, tools, &opts.thinking);
            let mut extra_headers = vec![];
            if let Some(cache_key) = cache_key {
                extra_headers.push(("x-affinity", cache_key.as_str()));
            }
            self.compat
                .do_stream(model, &extra_headers, &wire, event_tx, &auth)
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
            self.compat
                .fetch_and_parse_models(&auth, |m| {
                    // Filter: only completion_chat capable models
                    let has_completion_chat = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("completion_chat"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !has_completion_chat {
                        return None;
                    }

                    // Parse with Mistral-specific field names
                    let id = m["id"].as_str()?;
                    let context_window = m["max_context_length"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok());
                    let supports_thinking = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("reasoning"))
                        .and_then(Value::as_bool);
                    let supports_vision = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("vision"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Some(crate::model::ModelInfo {
                        id: id.to_string(),
                        context_window,
                        window_excludes_output: false,
                        max_output_tokens: None,
                        pricing: None,
                        supports_thinking,
                        supports_vision: Some(supports_vision),
                        reasoning_options: None,
                        provider_info: None,
                    })
                })
                .await
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        adjust_model(model);
    }
}

pub(crate) fn adjust_model(model: &mut Model) {
    if model.id.starts_with("ministral-") {
        model.thinking_override = Some(ThinkingSupport::No);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::lookup_entry;
    use crate::providers::Timeouts;
    use crate::{ContentBlock, Role};
    use serde_json::{Value, json};
    use test_case::test_case;

    const API_KEY: &str = "sk-mistral";
    const MODEL: &str = "mistral/mistral-medium-latest";
    const SYSTEM_PROMPT: &str = "sys";
    const REASONING: &str = "thinking";
    const REPLY: &str = "text";

    #[test_case("mistral-medium-latest", 0.15 ; "medium")]
    #[test_case("mistral-small-latest", 0.015 ; "small")]
    #[test_case("ministral-14b-2512", 0.02 ; "ministral")]
    #[test_case("mistral-large-2512", 0.05 ; "large_3")]
    fn cached_input_pricing(id: &str, expected: f64) {
        assert_eq!(
            lookup_entry(models(), id).unwrap().pricing.cache_read,
            expected
        );
    }

    #[test_case("mistral-large-latest", "mistral-large-2512" ; "large_latest_is_large_3")]
    #[test_case("mistral-large-4-0", LARGE_4 ; "large_4_keeps_its_entry")]
    #[test_case("zai-glm-5", "zai-glm-5-3" ; "glm_5_alias_is_glm_5_3")]
    #[test_case("zai-glm-latest", "zai-glm-5-3" ; "glm_latest_alias_is_glm_5_3")]
    #[test_case("zai-glm-5-2", "zai-glm-5-2" ; "glm_5_2_keeps_its_entry")]
    #[test_case("zai-glm-5-3-flash", "zai-glm-5-3-flash" ; "glm_5_3_flash_keeps_its_entry")]
    #[test_case("mistral-medium-3", "mistral-medium-3-5" ; "medium_3_alias_is_medium_3_5")]
    fn api_names_resolve_to_their_model(id: &str, model: &str) {
        assert!(
            lookup_entry(models(), id)
                .unwrap()
                .prefixes
                .contains(&model)
        );
    }

    #[test_case("zai-glm-5-3-flash", 0.15, 0.03, 0.50, true ; "flash")]
    #[test_case("zai-glm-5-3", 1.40, 0.14, 4.40, false ; "full")]
    #[test_case("deepseek-v4-1-flash-1017", 0.30, 0.007, 1.20, false ; "deepseek_flash")]
    fn hosted_models_have_distinct_metadata(
        id: &str,
        input: f64,
        cache_read: f64,
        output: f64,
        vision: bool,
    ) {
        let entry = lookup_entry(models(), id).unwrap();
        assert!(entry.prefixes.contains(&id));
        assert_eq!(entry.pricing.input, input);
        assert_eq!(entry.pricing.cache_read, cache_read);
        assert_eq!(entry.pricing.output, output);
        assert_eq!(entry.vision, vision);
        assert_eq!(entry.context_window, 1_000_000);
        assert_eq!(entry.max_output_tokens, None);
        assert!(!entry.default);
    }

    fn thinking_part() -> Value {
        json!({"type": "thinking", "thinking": [{"type": "text", "text": REASONING}]})
    }

    #[test_case(
        vec![ContentBlock::thinking(REASONING.into(), None), ContentBlock::Text { text: REPLY.into() }],
        json!([thinking_part(), {"type": "text", "text": REPLY}])
        ; "assistant_text_and_thinking"
    )]
    #[test_case(
        vec![ContentBlock::thinking(REASONING.into(), None)],
        json!([thinking_part()])
        ; "assistant_empty_content_with_thinking"
    )]
    #[test_case(
        vec![ContentBlock::Text { text: REPLY.into() }],
        json!(REPLY)
        ; "assistant_text_only_no_thinking"
    )]
    fn assistant_reasoning_travels_as_thinking_content(
        content: Vec<ContentBlock>,
        expected: Value,
    ) {
        let provider = Mistral::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer(API_KEY))),
            Timeouts::default(),
        );
        let reply = Message {
            role: Role::Assistant,
            content,
            ..Message::default()
        };

        let wire = provider
            .wire_request(
                &Model::from_spec(MODEL).unwrap(),
                &[reply],
                SYSTEM_PROMPT,
                &Value::Null,
                &RequestOptions::default(),
                None,
            )
            .unwrap();

        assert_eq!(
            wire.body["messages"],
            json!([
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "assistant", "content": expected},
            ])
        );
    }

    #[test_case("mistral/ministral-14b-latest", false ; "ministral_no_thinking")]
    #[test_case("mistral/mistral-medium-latest", true ; "mistral_medium_supports_thinking")]
    fn adjust_model_sets_thinking_support(spec: &str, expected: bool) {
        let mut model = Model::from_spec(spec).unwrap();
        adjust_model(&mut model);
        assert_eq!(model.supports_thinking(), expected);
    }
}
