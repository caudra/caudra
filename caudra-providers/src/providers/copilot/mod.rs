use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use futures_lite::io::BufReader;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::anthropic::shared;
use super::openai::responses;
use super::openai_compat;
use crate::model::{
    Billing, Model, ModelEntry, ModelFamily, ModelInfo, ModelPricing, StaticReasoningOption,
    lookup_entry,
};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, ReasoningOption, ReasoningOptions,
    RequestOptions, StreamResponse, ThinkingConfig,
};

pub mod auth;

const DEFAULT_API_ENDPOINT: &str = "https://api.githubcopilot.com";

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "copilot",
    display_name: "Copilot",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: DEFAULT_API_ENDPOINT,
    default_api_key_env: "GH_COPILOT_TOKEN",
    default_model: "copilot/gpt-5.6-terra",
    plans: None,
    login_url: Some("https://github.com/settings/copilot"),
    needs_url: false,
});
const GRAPHQL_QUERY: &str = "query { viewer { copilotEndpoints { api } } }";
const API_VERSION_HEADER: &str = "2025-10-01";
const EDITOR_VERSION_HEADER: &str = concat!("Caudra/", env!("CARGO_PKG_VERSION"));
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const MESSAGES_PATH: &str = "/v1/messages";
const MODELS_PATH: &str = "/models";

/// OpenAI families Copilot serves over the Responses API. Matched as substrings
/// so `gpt-6-sol` and `gpt-5.3-codex` both route without naming every release.
const RESPONSES_FAMILIES: &[&str] = &["gpt-5", "gpt-6", "codex"];

/// Scales `/models` AI-credit prices (1 credit = $0.01) to USD per 1M tokens.
const AIC_TO_USD_PER_MILLION: f64 = 10_000.0;

/// Fallback pricing used until `/models` reports `billing.token_prices` (or
/// for offline runs). The API wins via discovered metadata; these mirror
/// GitHub's published rates (usage-based billing since June 2026,
/// docs.github.com/copilot/reference/copilot-billing/models-and-pricing), at
/// the default context tier.
/// Levels these models declare, matching the models.dev catalog. Copilot's
/// `/models` response overrides these the moment it lands; the table only has
/// to survive a cold start.
const EFFORT_WITH_MAX: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high", "xhigh", "max",
])];
const EFFORT_TO_XHIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high", "xhigh",
])];
const EFFORT_TO_XHIGH_NO_NONE: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "low", "medium", "high", "xhigh",
])];
const EFFORT_TO_HIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high",
])];
const EFFORT_TO_HIGH_NO_NONE: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "medium", "high"])];
const KIMI_EFFORT: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "high", "max"])];
/// Claude on Copilot reasons unconditionally: no `none`, no toggle.
const CLAUDE_EFFORT_MAX: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "low", "medium", "high", "xhigh", "max",
])];
const CLAUDE_BUDGET: &[StaticReasoningOption] = &[StaticReasoningOption::BudgetTokens {
    min: Some(1_024),
    max: Some(32_000),
}];
const GEMINI_FLASH_24K: &[StaticReasoningOption] = &[
    StaticReasoningOption::Effort(&["minimal", "low", "medium", "high"]),
    StaticReasoningOption::BudgetTokens {
        min: Some(256),
        max: Some(24_000),
    },
];
const GEMINI_FLASH_32K: &[StaticReasoningOption] = &[
    StaticReasoningOption::Effort(&["minimal", "low", "medium", "high"]),
    StaticReasoningOption::BudgetTokens {
        min: Some(256),
        max: Some(32_000),
    },
];
const GEMINI_PRO_32K: &[StaticReasoningOption] = &[
    StaticReasoningOption::Effort(&["low", "medium", "high"]),
    StaticReasoningOption::BudgetTokens {
        min: Some(256),
        max: Some(32_000),
    },
];

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["gpt-5-mini"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.25,
                output: 2.00,
                cache_write: 0.00,
                cache_read: 0.025,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.4-mini"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 4.50,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.4-nano"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.25,
                cache_write: 0.00,
                cache_read: 0.02,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["claude-haiku-4.5"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_BUDGET),
        },
        ModelEntry {
            prefixes: &["gemini-3.5-flash"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.50,
                output: 9.00,
                cache_write: 0.00,
                cache_read: 0.15,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(65_536),
            context_window: 200_000,
            reasoning_options: Some(GEMINI_FLASH_24K),
        },
        ModelEntry {
            prefixes: &["gemini-3.6-flash"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 3.75,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(65_536),
            context_window: 200_000,
            reasoning_options: Some(GEMINI_FLASH_32K),
        },
        ModelEntry {
            prefixes: &["gemini-3.7-flash"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 3.75,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(65_536),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["mai-code-1-flash-picker"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 4.50,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4.5", "claude-sonnet-4.6"],
            small: false,
            family: ModelFamily::Generic,
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
            max_output_tokens: Some(64_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_BUDGET),
        },
        ModelEntry {
            prefixes: &["claude-sonnet-5"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_EFFORT_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.5"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["kimi-k2.7-code"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.95,
                output: 4.00,
                cache_write: 0.00,
                cache_read: 0.19,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["kimi-k3"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 0.00,
                cache_read: 0.30,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(KIMI_EFFORT),
        },
        ModelEntry {
            prefixes: &["gemini-3.1-pro-preview"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 12.00,
                cache_write: 0.00,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(65_536),
            context_window: 200_000,
            reasoning_options: Some(GEMINI_PRO_32K),
        },
        ModelEntry {
            prefixes: &["gpt-6-luna"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.50,
                cache_write: 0.125,
                cache_read: 0.01,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-6-sol"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            small: true,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.20,
                cache_write: 0.25,
                cache_read: 0.02,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.4"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.50,
                output: 15.00,
                cache_write: 0.00,
                cache_read: 0.25,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 12.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.3-codex"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["claude-opus-5.5"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 4.00,
                output: 20.00,
                cache_write: 5.00,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_EFFORT_MAX),
        },
        ModelEntry {
            prefixes: &[
                "claude-opus-5",
                "claude-opus-4.8",
                "claude-opus-4.7",
                "claude-opus-4.6",
                "claude-opus-4.5",
            ],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(64_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_EFFORT_MAX),
        },
        ModelEntry {
            prefixes: &["claude-opus-4.8-fast", "claude-fable-5"],
            small: false,
            family: ModelFamily::Generic,
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
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(CLAUDE_EFFORT_MAX),
        },
        ModelEntry {
            prefixes: &["grok-4.5"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 6.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["grok-4.6"],
            small: false,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 6.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_XHIGH_NO_NONE),
        },
    ];
    MODELS
}

pub struct Copilot {
    client: HttpClient,
    stream_timeout: Duration,
    auth: Arc<Mutex<Option<CopilotAuth>>>,
    resolved_auth: Option<Arc<Mutex<super::ResolvedAuth>>>,
    system_prefix: Option<String>,
    models: Arc<Mutex<HashMap<String, CopilotModel>>>,
}

impl Copilot {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        auth::load_token()?;
        Ok(Self {
            client: super::http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth: Arc::default(),
            resolved_auth: None,
            system_prefix: None,
            models: Arc::default(),
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth: Arc::default(),
            resolved_auth: Some(auth),
            system_prefix: None,
            models: Arc::default(),
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn auth(&self) -> Result<CopilotAuth, AgentError> {
        if let Some(auth) = &self.resolved_auth {
            return copilot_auth_from_resolved(&auth.lock().unwrap());
        }

        if let Some(auth) = self.auth.lock().unwrap().clone() {
            return Ok(auth);
        }

        let creds = auth::load_token()?;
        let host = creds.host.as_deref().unwrap_or("github.com");
        let endpoint =
            discover_api_endpoint(&self.client, &creds.api_key, &auth::graphql_url(host)).await;
        let auth = CopilotAuth {
            token: creds.api_key,
            endpoint,
        };
        *self.auth.lock().unwrap() = Some(auth.clone());
        Ok(auth)
    }

    async fn model_endpoint(&self, model_id: &str) -> Result<Endpoint, AgentError> {
        if let Some(model) = self.models.lock().unwrap().get(model_id).cloned() {
            return Ok(model.endpoint());
        }

        let models = self.fetch_models().await?;
        let mut guard = self.models.lock().unwrap();
        guard.clear();
        guard.extend(models.into_iter().map(|model| (model.id.clone(), model)));
        Ok(guard
            .get(model_id)
            .map(CopilotModel::endpoint)
            .unwrap_or_else(|| guess_endpoint(model_id)))
    }

    async fn fetch_models(&self) -> Result<Vec<CopilotModel>, AgentError> {
        let auth = self.auth().await?;
        let request = copilot_request(
            Request::builder()
                .method("GET")
                .uri(format!("{}{MODELS_PATH}", auth.endpoint)),
            &auth,
            None,
        )
        .body(())?;

        let mut response = self.client.send_async(request).await?;
        if !response.status().is_success() {
            return Err(AgentError::from_response(response).await);
        }

        let body: CopilotModelsResponse = serde_json::from_str(&response.text().await?)?;
        let mut models = body
            .data
            .into_iter()
            .filter_map(
                |value| match serde_json::from_value::<CopilotModel>(value) {
                    Ok(model) => Some(model),
                    Err(err) => {
                        warn!(error = %err, "skipping malformed Copilot model metadata");
                        None
                    }
                },
            )
            .filter(CopilotModel::is_enabled_chat_model)
            .collect::<Vec<_>>();

        if let Some(default_pos) = models.iter().position(|model| model.is_chat_default) {
            let default_model = models.remove(default_pos);
            models.insert(0, default_model);
        }

        Ok(models)
    }

    async fn stream_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let wire_tools = openai_compat::convert_tools(tools);
        let mut body = json!({
            "model": model.id,
            "messages": openai_compat::convert_messages(messages, system),
            "n": 1,
            "stream": true,
            "temperature": 0.1,
        });
        if wire_tools.as_array().is_some_and(|tools| !tools.is_empty()) {
            body["tools"] = wire_tools;
        }

        let request = self
            .build_post(
                &auth,
                CHAT_COMPLETIONS_PATH,
                Some("conversation-agent"),
                &body,
            )?
            .body(serde_json::to_vec(&body)?)?;
        let response = self.client.send_async(request).await?;
        if response.status().is_success() {
            openai_compat::parse_sse(
                BufReader::new(response.into_body()),
                event_tx,
                self.stream_timeout,
            )
            .await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn stream_responses(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let mut body = responses::build_body(model, messages, system, tools);
        responses::apply_responses_reasoning(&mut body, &thinking, model);
        let resolved = super::ResolvedAuth {
            base_url: Some(auth.endpoint.clone()),
            headers: copilot_headers(&auth, Some("conversation-agent")),
        };
        responses::do_stream(
            &self.client,
            model,
            &body,
            event_tx,
            &resolved,
            self.stream_timeout,
        )
        .await
    }

    async fn stream_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.auth().await?;
        let mut body = json!({
            "model": model.id,
            "max_tokens": model.max_output_tokens.unwrap_or(shared::FALLBACK_MAX_TOKENS),
            "system": [{"type": "text", "text": system}],
            "messages": anthropic_messages(messages),
            "tools": tools,
            "stream": true,
        });
        thinking.apply_to_body(&mut body, model);

        let request = self
            .build_post(&auth, MESSAGES_PATH, Some("conversation-agent"), &body)?
            .header("anthropic-version", "2023-06-01")
            .body(serde_json::to_vec(&body)?)?;
        let response = self.client.send_async(request).await?;
        if response.status().is_success() {
            super::anthropic::parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    fn build_post(
        &self,
        auth: &CopilotAuth,
        path: &str,
        interaction_type: Option<&str>,
        body: &Value,
    ) -> Result<isahc::http::request::Builder, AgentError> {
        debug!(
            path,
            body_bytes = serde_json::to_vec(body)?.len(),
            "sending Copilot API request"
        );
        Ok(copilot_request(
            Request::builder()
                .method("POST")
                .uri(format!("{}{path}", auth.endpoint)),
            auth,
            interaction_type,
        ))
    }
}

#[derive(Clone)]
struct CopilotAuth {
    token: String,
    endpoint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
}

#[derive(Clone, Deserialize)]
struct CopilotModel {
    id: String,
    #[serde(default)]
    policy: Option<CopilotModelPolicy>,
    #[serde(default)]
    capabilities: CopilotModelCapabilities,
    #[serde(default)]
    billing: CopilotModelBilling,
    #[serde(default)]
    is_chat_default: bool,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    supported_endpoints: Vec<String>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelBilling {
    #[serde(default)]
    token_prices: Option<CopilotTokenPrices>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotTokenPrices {
    #[serde(default)]
    batch_size: u32,
    #[serde(default)]
    default: Option<CopilotTokenPriceTier>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotTokenPriceTier {
    #[serde(default)]
    input_price: f64,
    #[serde(default)]
    output_price: f64,
    #[serde(default)]
    cache_price: f64,
}

impl CopilotModel {
    fn is_enabled_chat_model(&self) -> bool {
        self.model_picker_enabled
            && self.capabilities.model_type == "chat"
            && self
                .policy
                .as_ref()
                .is_none_or(|policy| policy.state == "enabled")
    }

    fn model_info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            context_window: self.capabilities.limits.max_context_window_tokens,
            max_output_tokens: self.capabilities.limits.max_output_tokens,
            pricing: self.pricing(),
            supports_thinking: Some(self.supports_thinking()),
            supports_vision: Some(self.capabilities.supports.vision),
            reasoning_options: Some(self.reasoning_options()),
            provider_info: None,
        }
    }

    /// The chat completions body carries no reasoning field, so declaring
    /// thinking there would offer the user a setting the request drops.
    fn supports_thinking(&self) -> bool {
        let supports = &self.capabilities.supports;
        self.endpoint() != Endpoint::ChatCompletions
            && (!supports.reasoning_effort.is_empty()
                || supports.adaptive_thinking
                || supports.max_thinking_budget.is_some()
                || supports.min_thinking_budget.is_some())
    }

    /// `/models` reports the levels and the budget window a model takes, which
    /// is exactly what [`ReasoningOptions`] describes.
    fn reasoning_options(&self) -> ReasoningOptions {
        let supports = &self.capabilities.supports;
        let mut options = Vec::new();
        if !supports.reasoning_effort.is_empty() {
            options.push(ReasoningOption::Effort {
                values: supports.reasoning_effort.clone(),
            });
        }
        if supports.min_thinking_budget.is_some() || supports.max_thinking_budget.is_some() {
            options.push(ReasoningOption::BudgetTokens {
                min: supports.min_thinking_budget,
                max: supports.max_thinking_budget,
            });
        }
        ReasoningOptions::new(options)
    }

    /// `/models` reports prices in AI credits per billing batch (1 credit =
    /// $0.01), scaled to USD per 1M tokens for [`ModelPricing`]. The endpoint
    /// exposes only the default context tier and cached-input reads; cache
    /// writes are inherited from the static manifest by id prefix so cost
    /// accounting matches the offline path.
    fn pricing(&self) -> Option<ModelPricing> {
        let token_prices = self.billing.token_prices.as_ref()?;
        let default = token_prices.default.as_ref()?;
        let batch_size = f64::from(token_prices.batch_size);
        if batch_size == 0.0 {
            return None;
        }
        let usd_per_million = AIC_TO_USD_PER_MILLION / batch_size;
        let manifest_cache_write =
            lookup_entry(models(), &self.id).map_or(0.0, |entry| entry.pricing.cache_write);
        Some(ModelPricing {
            input: default.input_price * usd_per_million,
            output: default.output_price * usd_per_million,
            cache_read: default.cache_price * usd_per_million,
            cache_write: manifest_cache_write,
            fast: None,
            tiers: Vec::new(),
        })
    }

    fn endpoint(&self) -> Endpoint {
        if self
            .supported_endpoints
            .iter()
            .any(|endpoint| endpoint == MESSAGES_PATH)
        {
            Endpoint::Messages
        } else if self
            .supported_endpoints
            .iter()
            .any(|endpoint| endpoint == RESPONSES_PATH)
        {
            Endpoint::Responses
        } else {
            Endpoint::ChatCompletions
        }
    }
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelPolicy {
    #[serde(default)]
    state: String,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelCapabilities {
    #[serde(default, rename = "type")]
    model_type: String,
    #[serde(default)]
    limits: CopilotModelLimits,
    #[serde(default)]
    supports: CopilotModelSupports,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelLimits {
    max_context_window_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

#[derive(Clone, Default, Deserialize)]
struct CopilotModelSupports {
    #[serde(default)]
    reasoning_effort: Vec<String>,
    #[serde(default)]
    adaptive_thinking: bool,
    max_thinking_budget: Option<u32>,
    min_thinking_budget: Option<u32>,
    #[serde(default)]
    vision: bool,
}

#[derive(Deserialize)]
struct CopilotModelsResponse {
    #[serde(default)]
    data: Vec<Value>,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
}

#[derive(Deserialize)]
struct GraphQlData {
    viewer: GraphQlViewer,
}

#[derive(Deserialize)]
struct GraphQlViewer {
    #[serde(rename = "copilotEndpoints")]
    copilot_endpoints: GraphQlCopilotEndpoints,
}

#[derive(Deserialize)]
struct GraphQlCopilotEndpoints {
    api: String,
}

async fn discover_api_endpoint(client: &HttpClient, token: &str, graphql_url: &str) -> String {
    match try_discover_api_endpoint(client, token, graphql_url).await {
        Ok(endpoint) => endpoint,
        Err(err) => {
            warn!(error = %err, fallback = DEFAULT_API_ENDPOINT, "Copilot endpoint discovery failed");
            DEFAULT_API_ENDPOINT.to_owned()
        }
    }
}

async fn try_discover_api_endpoint(
    client: &HttpClient,
    token: &str,
    graphql_url: &str,
) -> Result<String, AgentError> {
    let body = json!({ "query": GRAPHQL_QUERY });
    let request = Request::builder()
        .method("POST")
        .uri(graphql_url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("user-agent", super::user_agent())
        .body(serde_json::to_vec(&body)?)?;

    let mut response = client.send_async(request).await?;
    if !response.status().is_success() {
        return Err(AgentError::from_response(response).await);
    }

    let parsed: GraphQlResponse = serde_json::from_str(&response.text().await?)?;
    parsed
        .data
        .map(|data| data.viewer.copilot_endpoints.api)
        .ok_or_else(|| AgentError::Config {
            message: "Copilot endpoint discovery response contained no data".into(),
        })
}

fn copilot_request(
    builder: isahc::http::request::Builder,
    auth: &CopilotAuth,
    interaction_type: Option<&str>,
) -> isahc::http::request::Builder {
    let builder = builder
        .header("authorization", format!("Bearer {}", auth.token))
        .header("content-type", "application/json")
        .header("editor-version", EDITOR_VERSION_HEADER)
        .header("x-github-api-version", API_VERSION_HEADER)
        .header("user-agent", super::user_agent());

    if let Some(interaction_type) = interaction_type {
        builder
            .header("x-initiator", "agent")
            .header("x-interaction-type", interaction_type)
            .header("openai-intent", interaction_type)
    } else {
        builder
    }
}

fn copilot_headers(auth: &CopilotAuth, interaction_type: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {}", auth.token)),
        ("content-type".into(), "application/json".into()),
        ("editor-version".into(), EDITOR_VERSION_HEADER.into()),
        ("x-github-api-version".into(), API_VERSION_HEADER.into()),
    ];
    if let Some(interaction_type) = interaction_type {
        headers.extend([
            ("x-initiator".into(), "agent".into()),
            ("x-interaction-type".into(), interaction_type.into()),
            ("openai-intent".into(), interaction_type.into()),
        ]);
    }
    headers
}

fn copilot_auth_from_resolved(auth: &super::ResolvedAuth) -> Result<CopilotAuth, AgentError> {
    let token = auth
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.strip_prefix("Bearer "))
        .map(str::to_owned)
        .ok_or_else(|| AgentError::Config {
            message: "dynamic Copilot provider missing Bearer authorization header".into(),
        })?;

    Ok(CopilotAuth {
        token,
        endpoint: auth
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_API_ENDPOINT.into()),
    })
}

fn anthropic_messages(messages: &[Message]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "role": message.role,
                    "content": message.content,
                })
            })
            .collect(),
    )
}

fn guess_endpoint(model_id: &str) -> Endpoint {
    if model_id.starts_with("claude-") {
        Endpoint::Messages
    } else if RESPONSES_FAMILIES.iter().any(|f| model_id.contains(f)) {
        Endpoint::Responses
    } else {
        Endpoint::ChatCompletions
    }
}

impl Provider for Copilot {
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
            let mut prefixed_system = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut prefixed_system);
            let endpoint = self.model_endpoint(&model.id).await?;
            debug!(model = %model.id, ?endpoint, "running Copilot request");
            match endpoint {
                Endpoint::ChatCompletions => {
                    self.stream_chat_completions(model, messages, system, tools, event_tx)
                        .await
                }
                Endpoint::Responses => {
                    self.stream_responses(model, messages, system, tools, event_tx, opts.thinking)
                        .await
                }
                Endpoint::Messages => {
                    self.stream_messages(model, messages, system, tools, event_tx, opts.thinking)
                        .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let models = self.fetch_models().await?;
            let infos = models
                .iter()
                .map(CopilotModel::model_info)
                .collect::<Vec<_>>();
            let mut guard = self.models.lock().unwrap();
            guard.clear();
            guard.extend(models.into_iter().map(|model| (model.id.clone(), model)));
            Ok(infos)
        })
    }

    fn reasoning_transport(&self, model: &Model) -> crate::ReasoningTransport {
        let endpoint = self
            .models
            .lock()
            .unwrap()
            .get(&model.id)
            .map(CopilotModel::endpoint)
            .unwrap_or_else(|| guess_endpoint(&model.id));
        match endpoint {
            Endpoint::Messages => crate::ReasoningTransport::AnthropicMessages,
            Endpoint::Responses => crate::ReasoningTransport::OpenAiResponses,
            Endpoint::ChatCompletions => crate::ReasoningTransport::OpenAiChatCompletions,
        }
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            *self.auth.lock().unwrap() = None;
            self.models.lock().unwrap().clear();
            Ok(())
        })
    }

    /// Copilot sells no metered API, so its rates only ever describe what the
    /// same tokens would cost elsewhere. Unconditional, with no auth to read.
    fn adjust_model(&self, model: &mut Model) {
        model.billing = Billing::Subscription;
    }
}

#[cfg(test)]
mod tests {
    const OPUS_CACHE_WRITE: f64 = 6.25;

    use super::*;
    use crate::TokenUsage;
    use crate::providers::ResolvedAuth;
    use test_case::test_case;

    #[test]
    fn endpoint_prefers_messages_then_responses_then_chat() {
        let mut model = CopilotModel {
            id: "claude-sonnet-4.5".into(),
            policy: None,
            capabilities: CopilotModelCapabilities {
                model_type: "chat".into(),
                ..Default::default()
            },
            billing: CopilotModelBilling::default(),
            is_chat_default: false,
            model_picker_enabled: true,
            supported_endpoints: vec![CHAT_COMPLETIONS_PATH.into(), MESSAGES_PATH.into()],
        };
        assert_eq!(model.endpoint(), Endpoint::Messages);

        model.supported_endpoints = vec![RESPONSES_PATH.into()];
        assert_eq!(model.endpoint(), Endpoint::Responses);

        model.supported_endpoints.clear();
        assert_eq!(model.endpoint(), Endpoint::ChatCompletions);
    }

    #[test_case("claude-opus-5.5", Endpoint::Messages ; "claude_takes_messages")]
    #[test_case("gpt-6-sol", Endpoint::Responses ; "gpt_6_takes_responses")]
    #[test_case("gpt-6-luna", Endpoint::Responses ; "gpt_6_luna_takes_responses")]
    #[test_case("gpt-5.6-sol", Endpoint::Responses ; "gpt_5_takes_responses")]
    #[test_case("gpt-5.3-codex", Endpoint::Responses ; "codex_takes_responses")]
    #[test_case("gemini-3.1-pro-preview", Endpoint::ChatCompletions ; "everything_else_takes_chat")]
    fn guess_endpoint_covers_every_openai_family(model_id: &str, expected: Endpoint) {
        assert_eq!(guess_endpoint(model_id), expected);
    }

    #[test]
    fn parses_discovered_capabilities() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-5.6-sol",
            "model_picker_enabled": true,
            "supported_endpoints": ["/responses"],
            "capabilities": {
                "type": "chat",
                "limits": {
                    "max_context_window_tokens": 1_050_000,
                    "max_output_tokens": 128_000
                },
                "supports": {
                    "reasoning_effort": ["none", "low", "medium", "high"],
                    "adaptive_thinking": true,
                    "max_thinking_budget": 64_000,
                    "min_thinking_budget": 1_024,
                    "vision": true
                }
            }
        }))
        .unwrap();

        let info = model.model_info();
        assert_eq!(info.context_window, Some(1_050_000));
        assert_eq!(info.max_output_tokens, Some(128_000));
        assert_eq!(info.supports_thinking, Some(true));
        assert_eq!(info.supports_vision, Some(true));
        let options = info.reasoning_options.unwrap();
        assert_eq!(options.efforts(), ["none", "low", "medium", "high"]);
        assert_eq!(options.budget_bounds(), Some((Some(1_024), Some(64_000))));
    }

    #[test]
    fn an_unrecognized_picker_field_keeps_the_model() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-6",
            "model_picker_enabled": true,
            "model_picker_category": "reasoning",
            "capabilities": { "type": "chat" }
        }))
        .unwrap();

        assert!(model.is_enabled_chat_model());
    }

    #[test_case(RESPONSES_PATH, true; "responses honors reasoning")]
    #[test_case(MESSAGES_PATH, true; "messages honors thinking")]
    #[test_case(CHAT_COMPLETIONS_PATH, false; "chat completions drops reasoning")]
    fn thinking_support_follows_endpoint(endpoint: &str, expected: bool) {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "reasoner",
            "supported_endpoints": [endpoint],
            "capabilities": {
                "type": "chat",
                "supports": {"reasoning_effort": ["low", "high"], "adaptive_thinking": true}
            }
        }))
        .unwrap();

        assert_eq!(model.model_info().supports_thinking, Some(expected));
    }

    #[test_case(true, "gpt-5.6-luna"; "small defaults to luna")]
    #[test_case(false, "claude-opus-5"; "non_small_defaults_to_opus")]
    fn manifest_has_exactly_one_default_per_lane(small: bool, expected_prefix: &str) {
        let defaults: Vec<_> = models()
            .iter()
            .filter(|entry| entry.default && entry.small == small)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].prefixes[0], expected_prefix);
    }

    #[test_case("copilot/gpt-5.6-luna", 1_000_000, 1_000_000, 0.20 + 1.20; "luna default rates")]
    #[test_case("copilot/gpt-5.4-mini", 100_000, 100_000, 0.075 + 0.45; "gpt-5.4-mini beats gpt-5.4 prefix")]
    #[test_case("copilot/claude-opus-4.8-fast", 100_000, 100_000, 1.00 + 5.00; "opus 4.8 fast beats opus prefix")]
    #[test_case("copilot/claude-opus-5.5", 1_000_000, 1_000_000, 4.00 + 20.00; "opus 5.5 beats opus 5 prefix")]
    #[test_case("copilot/gpt-6-sol", 1_000_000, 1_000_000, 2.00 + 10.00; "gpt 6 sol has its own rates")]
    #[test_case("copilot/gpt-6-luna", 1_000_000, 1_000_000, 0.10 + 0.50; "gpt 6 luna has its own rates")]
    fn manifest_models_report_cost(spec: &str, input: u32, output: u32, expected: f64) {
        let usage = TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
        };
        let cost = Model::from_spec(spec)
            .unwrap()
            .list_cost(&usage, false)
            .unwrap();
        assert!((cost - expected).abs() < 1e-9);
    }

    /// Copilot's rates say what the same tokens would cost elsewhere. Reporting
    /// them as spend billed a subscriber for a bill that never arrives.
    #[test]
    fn copilot_models_are_always_covered_by_the_subscription() {
        let provider = Copilot::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer("test-token"))),
            crate::providers::Timeouts::default(),
        );
        let mut model = Model::from_spec("copilot/gpt-5.6-luna").unwrap();
        let baseline = model.pricing.clone();

        provider.adjust_model(&mut model);

        assert_eq!(model.billing, Billing::Subscription);
        assert_eq!(model.pricing.input, baseline.input);
        assert_eq!(model.pricing.output, baseline.output);
    }

    #[test]
    fn token_prices_convert_aic_per_batch_to_usd_per_million() {
        let model: CopilotModel = serde_json::from_value(json!({
            "id": "gpt-5",
            "billing": {
                "token_prices": {
                    "batch_size": 500_000,
                    "default": {"input_price": 500.0, "output_price": 3000.0, "cache_price": 50.0}
                }
            }
        }))
        .unwrap();

        let pricing = model.pricing().unwrap();
        assert_eq!(pricing.input, 10.0);
        assert_eq!(pricing.output, 60.0);
        assert_eq!(pricing.cache_read, 1.0);
        assert!(pricing.fast.is_none());
    }

    #[test]
    fn pricing_inherits_cache_write_from_manifest_by_prefix() {
        let billing = |id: &str| {
            json!({
                "id": id,
                "billing": {
                    "token_prices": {
                        "batch_size": 500_000,
                        "default": {"input_price": 250.0, "output_price": 1250.0, "cache_price": 25.0}
                    }
                }
            })
        };

        let opus: CopilotModel = serde_json::from_value(billing("claude-opus-5")).unwrap();
        let pricing = opus.pricing().unwrap();
        assert_eq!(pricing.input, 5.0);
        assert_eq!(pricing.output, 25.0);
        assert_eq!(pricing.cache_read, 0.5);
        assert_eq!(pricing.cache_write, OPUS_CACHE_WRITE);

        let unmatched: CopilotModel = serde_json::from_value(billing("gpt-5")).unwrap();
        assert_eq!(unmatched.pricing().unwrap().cache_write, 0.0);
    }

    #[test_case(json!({"id": "gpt-5"}) ; "no billing")]
    #[test_case(json!({"id": "gpt-5", "billing": {"token_prices": {"batch_size": 0, "default": {"input_price": 1.0, "output_price": 1.0, "cache_price": 0.1}}}}) ; "zero batch size")]
    fn pricing_falls_back_when_billing_unusable(billing: Value) {
        let model: CopilotModel = serde_json::from_value(billing).unwrap();
        assert!(model.pricing().is_none());
        assert!(model.model_info().pricing.is_none());
    }

    #[test_case(ThinkingConfig::Off, "none" ; "off is the declared none")]
    #[test_case(ThinkingConfig::Effort("medium".into()), "medium" ; "declared level passes through")]
    #[test_case(ThinkingConfig::Effort("max".into()), "xhigh" ; "undeclared level snaps down")]
    fn responses_reasoning_uses_effort_object(thinking: ThinkingConfig, expected: &str) {
        let model = Model::from_spec("copilot/gpt-5.4").unwrap();
        let mut body = json!({});
        responses::apply_responses_reasoning(&mut body, &thinking, &model);
        assert_eq!(
            body,
            json!({"reasoning": {"effort": expected, "summary": "auto"}})
        );
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn filters_enabled_chat_models() {
        let enabled = CopilotModel {
            id: "gpt-5.4".into(),
            policy: Some(CopilotModelPolicy {
                state: "enabled".into(),
            }),
            capabilities: CopilotModelCapabilities {
                model_type: "chat".into(),
                ..Default::default()
            },
            billing: CopilotModelBilling::default(),
            is_chat_default: false,
            model_picker_enabled: true,
            supported_endpoints: vec![],
        };
        assert!(enabled.is_enabled_chat_model());

        let disabled = CopilotModel {
            policy: Some(CopilotModelPolicy {
                state: "pending".into(),
            }),
            ..enabled
        };
        assert!(!disabled.is_enabled_chat_model());
    }
}
