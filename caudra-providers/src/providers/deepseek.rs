use std::sync::{Arc, Mutex};

use flume::Sender;
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::accounts::AccountCredential;
use crate::model::Model;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::types::{ProviderUsage, UsageLimit};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

const PAD: &str = "";
const THINKING_PAD_MARKERS: &[&str] = &["deepseek-v4", "deepseek-flash"];
const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "deepseek",
    base_url: "https://api.deepseek.com",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "DeepSeek",
};

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "deepseek",
    display_name: "DeepSeek",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.deepseek.com",
    default_api_key_env: "DEEPSEEK_API_KEY",
    default_model: "deepseek/deepseek-flash",
    plans: None,
    login_url: Some("https://platform.deepseek.com/api_keys"),
    needs_url: false,
});

#[derive(Deserialize)]
struct BalanceResponse {
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

impl From<BalanceResponse> for ProviderUsage {
    fn from(resp: BalanceResponse) -> Self {
        let limits = resp
            .balance_infos
            .into_iter()
            .map(|b| {
                let symbol = match b.currency.as_str() {
                    "USD" => "$",
                    "CNY" => "¥",
                    _ => "",
                };

                UsageLimit {
                    label: "Balance".into(),
                    percentage: None,
                    reset_at: None,
                    detail: Some(format!(
                        "total: {}{}, topped-up: {}{}, granted: {}{}",
                        symbol,
                        b.total_balance,
                        symbol,
                        b.topped_up_balance,
                        symbol,
                        b.granted_balance
                    )),
                    window: None,
                    length_secs: None,
                }
            })
            .collect();
        ProviderUsage { plan: None, limits }
    }
}

pub struct DeepSeek {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

impl DeepSeek {
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
        if thinking.is_enabled() {
            body["thinking"] = serde_json::json!({"type": "enabled"});
            thinking.apply_reasoning_effort(&mut body, model);
            pad_reasoning_content(&model.id, &mut body);
        } else {
            body["thinking"] = serde_json::json!({"type": "disabled"});
        }
        WireRequest::post(self.compat.chat_url(auth), body)
    }
}

impl Provider for DeepSeek {
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
            if matches!(opts.thinking, ThinkingConfig::Budget(_)) {
                warn!("DeepSeek reasoning does not support token budgets");
            }
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

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let body = self.compat.get_text(&auth, BALANCE_URL).await?;
            let parsed: BalanceResponse = serde_json::from_str(&body)?;
            Ok(Some(parsed.into()))
        })
    }
}

/// DeepSeek's reasoning models disagree about `reasoning_content`: V4 and
/// V4.1 Flash (`deepseek-flash`) in thinking mode want it on every assistant
/// turn (missing = 400), R1 refuses it as input. So we gate on their names,
/// the same trick Vercel's AI SDK uses, and back-fill the turns that have none
/// (plain replies, tool-only turns). The API only checks the field exists, so
/// `""` is enough.
///
/// Ref: <https://api-docs.deepseek.com/guides/thinking_mode>
fn pad_reasoning_content(model_id: &str, body: &mut Value) {
    if !THINKING_PAD_MARKERS
        .iter()
        .any(|marker| model_id.contains(marker))
    {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant")
            || msg
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some()
        {
            continue;
        }
        msg["reasoning_content"] = Value::String(PAD.to_string());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::manifest::ManifestRegistry;
    use crate::providers::Timeouts;
    use crate::{ContentBlock, Role};

    const V4: &str = "deepseek/deepseek-v4-pro";
    const FLASH: &str = "deepseek/deepseek-flash";
    const R1: &str = "deepseek/deepseek-reasoner";
    const API_KEY: &str = "sk-deepseek";
    const SYSTEM_PROMPT: &str = "sys";
    const USER_TEXT: &str = "hi";
    const REPLY: &str = "ok";
    const KEPT: &str = "kept";
    const CALL_ID: &str = "c1";
    const TOOL_NAME: &str = "read";
    const TOOL_OUTPUT: &str = "out";
    const PUBLISHED_OUTPUT_LIMIT: u32 = 384 * 1024;

    /// The hours and the surcharge as the pricing page states them.
    const PUBLISHED_PEAK_HOURS: &str = "2x during 01:00-04:00, 06:00-10:00 UTC";

    #[test_case(FLASH)]
    #[test_case(V4)]
    fn resolved_model_keeps_the_full_output_allowance(spec: &str) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.max_output_tokens, Some(PUBLISHED_OUTPUT_LIMIT));
    }

    #[test]
    fn the_manifest_bills_the_published_peak_hours() {
        let schedule = ManifestRegistry::get(CONFIG.slug)
            .expect("deepseek is a builtin")
            .pricing_schedule
            .expect("deepseek bills by the clock");
        assert_eq!(schedule.to_string(), PUBLISHED_PEAK_HOURS);
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Message::default()
        }
    }

    /// On the wire: system, user, the reasoned reply, the bare tool call, and
    /// its result.
    fn history() -> Vec<Message> {
        vec![
            Message::user(USER_TEXT.into()),
            assistant(vec![
                ContentBlock::thinking(KEPT.into(), None),
                ContentBlock::Text { text: REPLY.into() },
            ]),
            assistant(vec![ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({}))]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ID.into(),
                    content: TOOL_OUTPUT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            },
        ]
    }

    #[test_case(V4, ThinkingConfig::Adaptive, Some(PAD) ; "v4_thinking_pads_the_turn_without_reasoning")]
    #[test_case(FLASH, ThinkingConfig::Adaptive, Some(PAD) ; "v4_1_flash_thinking_pads_the_turn_without_reasoning")]
    #[test_case(V4, ThinkingConfig::Off, None ; "v4_without_thinking_is_not_padded")]
    #[test_case(R1, ThinkingConfig::Adaptive, None ; "r1_is_not_padded")]
    fn only_v4_thinking_pads_assistant_turns(
        spec: &str,
        thinking: ThinkingConfig,
        padded: Option<&str>,
    ) {
        let provider = DeepSeek::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer(API_KEY))),
            Timeouts::default(),
        );
        let wire = provider
            .wire_request(
                &Model::from_spec(spec).unwrap(),
                &history(),
                SYSTEM_PROMPT,
                &Value::Null,
                &RequestOptions {
                    thinking,
                    ..RequestOptions::default()
                },
                None,
            )
            .unwrap();

        let msgs = wire.body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["reasoning_content"], KEPT);
        assert_eq!(
            msgs[3].get("reasoning_content").and_then(Value::as_str),
            padded
        );
        for i in [0, 1, 4] {
            assert!(msgs[i].get("reasoning_content").is_none());
        }
    }
}
