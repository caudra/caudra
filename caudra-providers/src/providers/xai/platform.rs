use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use caudra_storage::log::{outcome, target};
use flume::Sender;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::accounts::{AccountCredential, AccountSecret};
use crate::error::{DETAIL_CAP, provider_detail};
use crate::model::Model;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::oauth::AccountStore;
use crate::providers::openai::responses;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, Timeouts};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
};

use super::{auth, catalog};

const UPGRADE_REQUIRED: u16 = 426;
const VERSION_REFUSED: &str = "xAI's Grok CLI proxy refused client version";
const PROXY_SAID: &str = "The proxy said:";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "xai",
    base_url: "https://api.x.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "xAI",
};

pub struct Xai {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    oauth: AtomicBool,
    store: Option<AccountStore>,
    system_prefix: Option<String>,
}

impl Xai {
    pub fn new(account: &AccountCredential, timeouts: Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(auth::account_auth(&account.secret)?)),
            oauth: AtomicBool::new(account.is_oauth()),
            store: AccountStore::for_account(account)?,
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            oauth: AtomicBool::new(false),
            store: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    fn is_oauth(&self) -> bool {
        self.oauth.load(Ordering::Relaxed)
    }

    /// Renews a token near expiry when nothing is rejected, and replaces a
    /// rejected one otherwise. Either way the tokens on disk win when another
    /// process already rotated them.
    async fn refresh_oauth(&self, rejected_accesses: Vec<String>) -> Result<(), AgentError> {
        let store = self.store.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let tokens =
            smol::unblock(move || auth::refresh_from_storage(&store, &rejected_accesses)).await?;
        let resolved = auth::build_oauth_resolved(&tokens);
        let mut current = self.auth.lock().unwrap();
        if *current != resolved {
            *current = resolved;
            info!(
                target: target::PROVIDER,
                event = crate::auth_events::REFRESHED,
                provider = auth::PROVIDER,
                outcome = outcome::OK,
                "refreshed OAuth token"
            );
        }
        Ok(())
    }

    async fn with_oauth_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let refreshable = self.is_oauth() && self.store.is_some();
        if refreshable {
            self.refresh_oauth(Vec::new()).await?;
        }
        let result = f().await;
        if refreshable
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth(self.rejected_access()).await.is_ok()
        {
            return f().await;
        }
        result
    }

    fn rejected_access(&self) -> Vec<String> {
        bearer_token(&self.current_auth()).into_iter().collect()
    }

    fn install_secret(&self, secret: &AccountSecret) -> Result<(), AgentError> {
        *self.auth.lock().unwrap() = auth::account_auth(secret)?;
        self.oauth
            .store(matches!(secret, AccountSecret::OAuth(_)), Ordering::Relaxed);
        Ok(())
    }

    /// What one attempt posts with `auth`, for the send and the dry run alike:
    /// the Responses API over a login, Chat Completions over a key.
    #[allow(clippy::too_many_arguments)]
    fn request(
        &self,
        oauth: bool,
        auth: &ResolvedAuth,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
        cache_key: Option<&CacheKey>,
    ) -> Result<WireRequest, AgentError> {
        let mut buf = String::new();
        let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);
        if !oauth {
            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking.apply_reasoning_effort(&mut body, model);
            return Ok(WireRequest::post(self.compat.chat_url(auth), body));
        }
        let mut body = responses::build_body(model, messages, system, tools);
        apply_grok_reasoning(&mut body, opts, model);
        responses::apply_prompt_cache_key(&mut body, cache_key);
        Ok(WireRequest::post(responses::responses_url(auth)?, body))
    }
}

fn apply_grok_reasoning(body: &mut Value, opts: &RequestOptions, model: &Model) {
    if !model.supports_thinking() {
        return;
    }
    body["reasoning"] = json!({ "summary": "auto" });
    if let Some(effort) = opts.thinking.effort_str(model) {
        body["reasoning"]["effort"] = json!(effort);
    }
    let include = body["include"].as_array_mut();
    match include {
        Some(arr) => {
            if !arr
                .iter()
                .any(|v| v.as_str() == Some(responses::ENCRYPTED_REASONING))
            {
                arr.push(json!(responses::ENCRYPTED_REASONING));
            }
        }
        None => body["include"] = json!([responses::ENCRYPTED_REASONING]),
    }
}

fn proxy_request_headers(model: &Model, cache_key: Option<&CacheKey>) -> Vec<(String, String)> {
    let session = cache_key.map(ToString::to_string).unwrap_or_else(random_id);
    vec![
        ("accept".into(), "text/event-stream".into()),
        ("x-grok-conv-id".into(), session.clone()),
        ("x-grok-session-id".into(), session),
        ("x-grok-req-id".into(), random_id()),
        (
            "x-grok-model-override".into(),
            model.id.to_ascii_lowercase(),
        ),
    ]
}

fn random_id() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

/// The proxy's own 426 text says to run `grok update`, which does nothing for
/// Caudra, so the remedy goes first and the proxy's minimum version after it.
fn explain_version_gate(error: AgentError) -> AgentError {
    match error {
        AgentError::Api {
            status: UPGRADE_REQUIRED,
            message,
            retry_after,
        } => {
            let remedy = format!(
                "{VERSION_REFUSED} {}; run `caudra update`, or set {} to the version it asks for",
                auth::client_version(),
                auth::CLIENT_VERSION_ENV,
            );
            let message = match provider_detail(&message, DETAIL_CAP) {
                Some(detail) => format!("{remedy}. {PROXY_SAID} {detail}"),
                None => remedy,
            };
            AgentError::Api {
                status: UPGRADE_REQUIRED,
                message,
                retry_after,
            }
        }
        error => error,
    }
}

impl Provider for Xai {
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
            let oauth = self.is_oauth();
            self.with_oauth_retry(|| async {
                let mut auth = self.current_auth();
                let wire = self.request(
                    oauth, &auth, model, messages, system, tools, &opts, cache_key,
                )?;
                if !oauth {
                    return self
                        .compat
                        .do_stream(model, &[], &wire, event_tx, &auth)
                        .await;
                }
                auth.headers.extend(proxy_request_headers(model, cache_key));
                responses::do_stream(
                    self.compat.client(),
                    model,
                    &wire,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await
                .map_err(explain_version_gate)
            })
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
        cache_key: Option<&CacheKey>,
    ) -> Result<WireRequest, AgentError> {
        self.request(
            self.is_oauth(),
            &self.current_auth(),
            model,
            messages,
            system,
            tools,
            opts,
            cache_key,
        )
    }

    fn reasoning_transport(&self, _model: &Model) -> crate::ReasoningTransport {
        if self.is_oauth() {
            crate::ReasoningTransport::OpenAiResponses
        } else {
            crate::ReasoningTransport::OpenAiChatCompletions
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                return self
                    .with_oauth_retry(|| async {
                        let auth = self.current_auth();
                        let access = bearer_token(&auth).ok_or_else(|| AgentError::Config {
                            message: "xAI OAuth token missing from resolved auth".into(),
                        })?;
                        smol::unblock(move || catalog::list_models(&access, false)).await
                    })
                    .await;
            }
            self.with_oauth_retry(|| async {
                let auth = self.current_auth();
                self.compat.do_list_models(&auth).await
            })
            .await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                self.refresh_oauth(self.rejected_access()).await
            } else {
                self.reload_auth().await
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(store) = self.store.clone() else {
                return Ok(());
            };
            let secret = smol::unblock(move || store.reload()).await?;
            self.install_secret(&secret)?;
            debug!("reloaded xAI auth from storage");
            Ok(())
        })
    }

    fn reload_auth_if_changed(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            let previous = self.current_auth();
            self.reload_auth().await?;
            Ok(previous != self.current_auth())
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        catalog::adjust_model(model);
    }
}

fn bearer_token(auth: &ResolvedAuth) -> Option<String> {
    auth.headers.iter().find_map(|(key, value)| {
        if !key.eq_ignore_ascii_case("authorization") {
            return None;
        }
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use caudra_storage::auth::OAuthTokens;
    use test_case::test_case;

    use super::*;
    use crate::ReasoningOptions;
    use crate::providers::openai::responses::RESPONSES_PATH;
    use crate::providers::openai_compat::CHAT_COMPLETIONS_PATH;
    use crate::providers::test_support::CREDENTIAL_IN_URL;
    use crate::types::ThinkingConfig;
    use crate::{ModelFamily, ModelPricing};

    const CACHE_KEY: &str = "session/task";
    const SYSTEM_PROMPT: &str = "You are Grok.";
    const LOGIN_ACCESS: &str = "xai-login-access";
    const LOGIN_REFRESH: &str = "xai-login-refresh";
    const API_KEY: &str = "xai-api-key";
    const TEST_BASE_URL: &str = "https://api.x.test/v1";
    const VERSION_GATE_DETAIL: &str =
        "Your Grok CLI version (1.0.50) is outdated. Please update to version 1.0.60 or later.";
    const VERSION_GATE_BODY: &str = r#"{"error":"Your Grok CLI version (1.0.50) is outdated. Please update to version 1.0.60 or later."}"#;

    fn provider_with_login() -> Xai {
        let tokens = OAuthTokens {
            access: LOGIN_ACCESS.into(),
            refresh: LOGIN_REFRESH.into(),
            expires: u64::MAX,
            account_id: None,
        };
        Xai {
            oauth: AtomicBool::new(true),
            ..Xai::with_auth(
                Arc::new(Mutex::new(auth::build_oauth_resolved(&tokens))),
                Timeouts::default(),
            )
        }
    }

    fn provider_with_key() -> Xai {
        Xai::with_auth(
            Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(TEST_BASE_URL.into()),
                ..ResolvedAuth::bearer(API_KEY)
            })),
            Timeouts::default(),
        )
    }

    fn dry_run(provider: &Xai, opts: &RequestOptions) -> WireRequest {
        provider
            .wire_request(
                &test_model(true),
                &[],
                SYSTEM_PROMPT,
                &Value::Null,
                opts,
                None,
            )
            .unwrap()
    }

    fn test_model(thinking: bool) -> Model {
        Model {
            id: "grok-4.6".into(),
            provider: "xai".into(),
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: Some(if thinking {
                crate::model::ThinkingSupport::Yes
            } else {
                crate::model::ThinkingSupport::No
            }),
            supports_vision_override: Some(true),
            supports_cache_breakpoints_override: None,
            supports_pdf: false,
            pricing: ModelPricing::ZERO,
            price_change: None,
            discovered_free: false,
            max_output_tokens: Some(131_072),
            context_window: 500_000,
            window_excludes_output: false,
            reasoning_options: ReasoningOptions::default(),
            thinking_fields: None,
            billing: crate::model::Billing::default(),
        }
    }

    #[test]
    fn grok_reasoning_sets_effort_and_include() {
        let wire = dry_run(
            &provider_with_login(),
            &RequestOptions {
                thinking: ThinkingConfig::Effort("high".into()),
                ..RequestOptions::default()
            },
        );

        assert_eq!(wire.body["reasoning"]["effort"], "high");
        assert_eq!(wire.body["reasoning"]["summary"], "auto");
        assert_eq!(wire.body["include"][0], responses::ENCRYPTED_REASONING);
    }

    /// A login speaks the Responses API through the CLI proxy, a key speaks
    /// Chat Completions to the public API.
    #[test_case(true, auth::CLI_BASE_URL, RESPONSES_PATH ; "a_login_posts_responses_to_the_cli_proxy")]
    #[test_case(false, TEST_BASE_URL, CHAT_COMPLETIONS_PATH ; "a_key_posts_chat_completions_to_the_api")]
    fn wire_request_posts_where_the_send_does(login: bool, base: &str, path: &str) {
        let provider = if login {
            provider_with_login()
        } else {
            provider_with_key()
        };

        let wire = dry_run(&provider, &RequestOptions::default());

        assert_eq!(wire.url, format!("{base}{path}"));
        for credential in [LOGIN_ACCESS, API_KEY] {
            assert!(!wire.url.contains(credential), "{CREDENTIAL_IN_URL}");
        }
    }

    #[test]
    fn grok_reasoning_without_effort_still_requests_summary() {
        let model = test_model(true);
        let mut body = json!({"model": "grok-4.6"});

        apply_grok_reasoning(&mut body, &RequestOptions::default(), &model);

        assert_eq!(body["reasoning"]["summary"], "auto");
        assert!(body["reasoning"].get("effort").is_none());
        assert_eq!(body["include"][0], responses::ENCRYPTED_REASONING);
    }

    #[test]
    fn grok_reasoning_skipped_when_model_has_no_thinking() {
        let model = test_model(false);
        let mut body = json!({"model": "grok-4.6"});
        apply_grok_reasoning(&mut body, &RequestOptions::default(), &model);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn proxy_conversation_headers_carry_the_cache_key() {
        let key = CacheKey::task(None, CACHE_KEY);
        let headers = proxy_request_headers(&test_model(true), Some(&key));

        for name in ["x-grok-conv-id", "x-grok-session-id"] {
            let value = headers.iter().find(|(header, _)| header == name);
            assert_eq!(value.map(|(_, value)| value.as_str()), Some(CACHE_KEY));
        }
    }

    #[test]
    fn bearer_token_extracts_access() {
        let auth = ResolvedAuth {
            base_url: None,
            headers: vec![("authorization".into(), "Bearer tok-123".into())],
        };
        assert_eq!(bearer_token(&auth).as_deref(), Some("tok-123"));
    }

    #[test]
    fn version_gate_names_the_override_before_the_proxy_minimum() {
        let error = explain_version_gate(AgentError::api(UPGRADE_REQUIRED, VERSION_GATE_BODY));

        let AgentError::Api {
            status, message, ..
        } = &error
        else {
            panic!("a 426 must stay an API error, got {error:?}");
        };
        assert_eq!(*status, UPGRADE_REQUIRED);
        assert!(message.starts_with(VERSION_REFUSED));
        assert!(message.contains(auth::CLIENT_VERSION_ENV));
        assert!(message.ends_with(&format!("{PROXY_SAID} {VERSION_GATE_DETAIL}")));
    }

    #[test_case(401 ; "auth_failure")]
    #[test_case(500 ; "server_error")]
    fn other_statuses_keep_the_proxy_message(status: u16) {
        let error = explain_version_gate(AgentError::api(status, VERSION_GATE_BODY));

        assert!(matches!(
            error,
            AgentError::Api { status: kept, ref message, .. }
                if kept == status && message == VERSION_GATE_BODY
        ));
    }
}
