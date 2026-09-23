use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ThinkingSupport};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "mistral",
    api_key_env: "MISTRAL_API_KEY",
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
            ],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.5,
                output: 7.5,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                tiers: ModelPricing::UNTIERED,
            },
            max_output_tokens: None,
            context_window: 262_144,
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
            prefixes: &["mistral-small-latest", "mistral-small-2603"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.00,
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
                cache_read: 0.00,
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
    key_pool: Option<KeyPool>,
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
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("mistral", CONFIG.api_key_env)?;
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

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        adjust_model(model);
    }
}

fn adjust_model(model: &mut Model) {
    if model.id.starts_with("ministral-") {
        model.thinking_override = Some(ThinkingSupport::No);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Timeouts;
    use crate::{ContentBlock, Role};
    use serde_json::{Value, json};
    use test_case::test_case;

    const API_KEY: &str = "sk-mistral";
    const MODEL: &str = "mistral/mistral-medium-latest";
    const SYSTEM_PROMPT: &str = "sys";
    const REASONING: &str = "thinking";
    const REPLY: &str = "text";

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
