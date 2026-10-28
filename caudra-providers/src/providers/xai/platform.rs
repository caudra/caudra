use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use caudra_storage::auth::now_millis;
use caudra_storage::log::{outcome, target};
use flume::Sender;
use jiff::Timestamp;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::accounts::{AccountCredential, AccountSecret};
use crate::error::{DETAIL_CAP, provider_detail};
use crate::model::Model;
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::oauth::{AccountStore, jwt_claims};
use crate::providers::openai::responses;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, Timeouts};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    UsageLimit,
};

use super::{auth, catalog, usage};

const UPGRADE_REQUIRED: u16 = 426;
const VERSION_REFUSED: &str = "xAI's Grok CLI proxy refused client version";
const PROXY_SAID: &str = "The proxy said:";
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const ROLLING_DAY_MS: u64 = 24 * 60 * 60 * 1000;
const TOKEN_COUNTS_MARKER: &str = "tokens (actual/limit): ";
const BILLING_ACCOUNT_CHANGED: &str =
    "xAI account changed while fetching usage; refresh usage again";

struct ObservedQuota {
    at: u64,
    limit: UsageLimit,
}

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
    observed_quotas: Mutex<BTreeMap<String, ObservedQuota>>,
}

impl Xai {
    pub fn new(account: &AccountCredential, timeouts: Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(auth::account_auth(&account.secret)?)),
            oauth: AtomicBool::new(account.is_oauth()),
            store: AccountStore::for_account(account)?,
            system_prefix: None,
            observed_quotas: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            oauth: AtomicBool::new(false),
            store: None,
            system_prefix: None,
            observed_quotas: Mutex::new(BTreeMap::new()),
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
        if self.install_auth(resolved) {
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
        self.install_auth(auth::account_auth(secret)?);
        self.oauth
            .store(matches!(secret, AccountSecret::OAuth(_)), Ordering::Relaxed);
        Ok(())
    }

    fn install_auth(&self, resolved: ResolvedAuth) -> bool {
        let mut current = self.auth.lock().unwrap();
        if *current == resolved {
            return false;
        }
        if !same_identity(&current, &resolved) {
            self.observed_quotas.lock().unwrap().clear();
        }
        *current = resolved;
        true
    }

    fn observe_result<T>(
        &self,
        model: &str,
        result: &Result<T, AgentError>,
        at: u64,
        auth: &ResolvedAuth,
    ) {
        let current = self.auth.lock().unwrap();
        if !same_identity(&current, auth) {
            return;
        }
        let mut observed = self.observed_quotas.lock().unwrap();
        observed.retain(|_, quota| at.saturating_sub(quota.at) < ROLLING_DAY_MS);
        match result {
            Ok(_) => {
                observed.remove(model);
            }
            Err(error) if error.is_free_usage_exhausted() => {
                observed.insert(model.to_owned(), observed_quota(model, error, at));
            }
            Err(_) => {}
        }
    }

    fn append_observed_quotas(
        &self,
        usage: &mut ProviderUsage,
        now: u64,
        auth: &ResolvedAuth,
    ) -> Result<(), AgentError> {
        let current = self.auth.lock().unwrap();
        if !same_identity(&current, auth) {
            return Err(AgentError::Config {
                message: BILLING_ACCOUNT_CHANGED.into(),
            });
        }
        let mut observed = self.observed_quotas.lock().unwrap();
        observed.retain(|_, quota| now.saturating_sub(quota.at) < ROLLING_DAY_MS);
        usage
            .limits
            .extend(observed.values().map(|quota| quota.limit.clone()));
        Ok(())
    }

    async fn fetch_usage_at(&self, url: &str) -> Result<Option<ProviderUsage>, AgentError> {
        if !self.is_oauth() {
            return Ok(None);
        }
        let (mut usage, auth) = self
            .with_oauth_retry(|| async {
                let mut auth = self.current_auth();
                if let Some(subject) = bearer_token(&auth)
                    .and_then(|token| jwt_claims(&token))
                    .and_then(|claims| claims.get("sub")?.as_str().map(str::to_owned))
                    .filter(|subject| !subject.is_empty())
                {
                    auth.headers.push(("x-userid".into(), subject));
                }
                let response = self.compat.get_text(&auth, url).await?;
                Ok((usage::parse_usage(&response)?, auth))
            })
            .await?;
        self.append_observed_quotas(&mut usage, now_millis(), &auth)?;
        Ok(Some(usage))
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
                let result = responses::do_stream(
                    self.compat.client(),
                    model,
                    &wire,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await
                .map_err(explain_version_gate);
                self.observe_result(&model.id, &result, now_millis(), &auth);
                result
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
        Box::pin(self.fetch_usage_at(BILLING_URL))
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

fn same_identity(left: &ResolvedAuth, right: &ResolvedAuth) -> bool {
    let left = bearer_token(left);
    let right = bearer_token(right);
    if left == right {
        return true;
    }
    let claims = |token: Option<String>| token.and_then(|token| jwt_claims(&token));
    let (Some(left), Some(right)) = (claims(left), claims(right)) else {
        return false;
    };
    left.get("sub")
        .and_then(Value::as_str)
        .is_some_and(|subject| !subject.is_empty())
        && ["sub", "iss", "principal_id", "principal_type"]
            .iter()
            .all(|key| left.get(key) == right.get(key))
}

fn observed_quota(model: &str, error: &AgentError, at: u64) -> ObservedQuota {
    let counts = match error {
        AgentError::Api { message, .. } => serde_json::from_str::<Value>(message)
            .ok()
            .and_then(|body| {
                let detail = body.get("error")?;
                let detail = detail
                    .as_str()
                    .or_else(|| detail.get("message")?.as_str())?;
                let (_, counts) = detail.split_once(TOKEN_COUNTS_MARKER)?;
                let (used, limit) = counts.split_once('/')?;
                let used = used.parse::<u64>().ok()?;
                let limit = limit
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>();
                let limit = limit.parse::<u64>().ok().filter(|limit| *limit > 0)?;
                Some(format!("{used}/{limit} tokens; "))
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let timestamp = i64::try_from(at)
        .ok()
        .and_then(|at| Timestamp::from_millisecond(at).ok())
        .map_or_else(|| at.to_string(), |timestamp| timestamp.to_string());
    ObservedQuota {
        at,
        limit: UsageLimit {
            label: format!("{model} free usage (observed)"),
            percentage: None,
            reset_at: None,
            detail: Some(format!(
                "{counts}exhausted at {timestamp}; rolling 24-hour window, reset time unknown"
            )),
            window: None,
            length_secs: None,
        },
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
    use std::time::Duration;

    use caudra_storage::auth::OAuthTokens;
    use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
    use smol::net::TcpListener;
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
    const QUOTA_BODY: &str = r#"{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage for model grok-4.7 for now. Usage resets over a rolling 24-hour window — tokens (actual/limit): 639471/600000. Upgrade for higher limits."}"#;
    const QUOTA_MODEL: &str = "grok-4.7";
    const OBSERVED_AT: u64 = 1_793_186_597_760;
    const BILLING_BODY: &str = r#"{"config":{"creditUsagePercent":76,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-11-03T19:07:54Z"}}}"#;
    const SUBJECT: &str = "quota-test-subject";
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

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

    fn quota_error() -> Result<(), AgentError> {
        Err(AgentError::api(429, QUOTA_BODY))
    }

    fn observed_limits(provider: &Xai, now: u64) -> Vec<UsageLimit> {
        let mut usage = ProviderUsage {
            plan: None,
            limits: Vec::new(),
        };
        provider
            .append_observed_quotas(&mut usage, now, &provider.current_auth())
            .unwrap();
        usage.limits
    }

    #[test]
    fn exhausted_quota_is_an_observation_not_a_reset_or_routing_window() {
        let provider = provider_with_login();
        provider.observe_result(
            QUOTA_MODEL,
            &quota_error(),
            OBSERVED_AT,
            &provider.current_auth(),
        );
        let limits = observed_limits(&provider, OBSERVED_AT);
        assert_eq!(limits.len(), 1);
        let limit = &limits[0];
        assert!(limit.label.contains(QUOTA_MODEL));
        let detail = limit.detail.as_deref().unwrap();
        assert!(detail.contains("639471/600000 tokens"));
        assert!(detail.contains("2026-10-28T"));
        assert!(detail.contains("reset time unknown"));
        assert_eq!(limit.percentage, None);
        assert_eq!(limit.reset_at, None);
        assert!(limit.quota_window().is_none());
        assert!(observed_limits(&provider_with_login(), OBSERVED_AT).is_empty());
    }

    #[test]
    fn observations_expire_and_success_clears_only_the_matching_model() {
        let provider = provider_with_login();
        let auth = provider.current_auth();
        provider.observe_result(QUOTA_MODEL, &quota_error(), OBSERVED_AT, &auth);
        provider.observe_result("grok-4.6", &Ok(()), OBSERVED_AT, &auth);
        provider.observe_result(
            QUOTA_MODEL,
            &Err::<(), _>(AgentError::api(500, "unavailable")),
            OBSERVED_AT,
            &auth,
        );
        assert_eq!(observed_limits(&provider, OBSERVED_AT).len(), 1);
        provider.observe_result(QUOTA_MODEL, &Ok(()), OBSERVED_AT, &auth);
        assert!(observed_limits(&provider, OBSERVED_AT).is_empty());
        provider.observe_result(QUOTA_MODEL, &quota_error(), OBSERVED_AT, &auth);
        assert!(observed_limits(&provider, OBSERVED_AT + ROLLING_DAY_MS).is_empty());
    }

    #[test]
    fn api_key_usage_never_contacts_subscription_billing() {
        assert!(
            smol::block_on(provider_with_key().fetch_usage_at("not a URL"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refreshed_identity_keeps_observations_but_replaced_identity_clears_them() {
        let provider = provider_with_login();
        let login = |subject: &str, expires: u64| {
            ResolvedAuth::bearer(&crate::providers::oauth::unsigned_jwt(&json!({
                "sub": subject, "exp": expires,
            })))
        };
        let original = login(SUBJECT, 1);
        provider.install_auth(original.clone());
        provider.observe_result(QUOTA_MODEL, &quota_error(), OBSERVED_AT, &original);
        provider.install_auth(login(SUBJECT, 2));
        assert_eq!(observed_limits(&provider, OBSERVED_AT).len(), 1);
        provider.install_auth(login("replacement", 2));
        assert!(observed_limits(&provider, OBSERVED_AT).is_empty());
        provider.observe_result(QUOTA_MODEL, &quota_error(), OBSERVED_AT, &original);
        assert!(observed_limits(&provider, OBSERVED_AT).is_empty());
        let replacement = provider.current_auth();
        provider.observe_result(QUOTA_MODEL, &quota_error(), OBSERVED_AT, &replacement);
        provider.observe_result(QUOTA_MODEL, &Ok(()), OBSERVED_AT, &original);
        assert_eq!(observed_limits(&provider, OBSERVED_AT).len(), 1);
        let mut usage = ProviderUsage {
            plan: None,
            limits: Vec::new(),
        };
        assert_eq!(
            provider
                .append_observed_quotas(&mut usage, OBSERVED_AT, &original)
                .unwrap_err()
                .to_string(),
            BILLING_ACCOUNT_CHANGED
        );
        assert!(usage.limits.is_empty());
    }

    #[test_case(200, BILLING_BODY, false ; "billing_success")]
    #[test_case(401, "{}", false ; "auth_error")]
    #[test_case(429, QUOTA_BODY, false ; "quota_error")]
    #[test_case(500, "{}", false ; "server_error")]
    #[test_case(200, "not json", false ; "malformed_response")]
    #[test_case(200, BILLING_BODY, true ; "replaced_account_rejects_inflight_billing")]
    fn oauth_usage_fetches_billing_with_account_headers(
        status: u16,
        body: &str,
        replace_identity: bool,
    ) {
        smol::block_on(smol::future::or(
            async {
                let provider = provider_with_login();
                let token = crate::providers::oauth::unsigned_jwt(&json!({"sub": SUBJECT}));
                provider
                    .auth
                    .lock()
                    .unwrap()
                    .headers
                    .retain(|(name, _)| name != "authorization");
                provider
                    .auth
                    .lock()
                    .unwrap()
                    .headers
                    .push(("authorization".into(), format!("Bearer {token}")));
                provider.observe_result(
                    QUOTA_MODEL,
                    &quota_error(),
                    now_millis(),
                    &provider.current_auth(),
                );
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!(
                    "http://{}/v1/billing?format=credits",
                    listener.local_addr().unwrap()
                );
                let server = async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut chunk = [0; 4096];
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let read = stream.read(&mut chunk).await.unwrap();
                        assert!(read > 0 && request.len() < 65536);
                        request.extend_from_slice(&chunk[..read]);
                    }
                    if replace_identity {
                        provider.install_auth(ResolvedAuth::bearer("replacement-account-token"));
                    }
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    String::from_utf8(request).unwrap().to_ascii_lowercase()
                };
                let (result, request) = futures::join!(provider.fetch_usage_at(&url), server);
                assert!(request.starts_with("get /v1/billing?format=credits "));
                assert!(request.contains(&format!(
                    "authorization: bearer {}",
                    token.to_ascii_lowercase()
                )));
                assert!(request.contains(&format!("x-userid: {SUBJECT}")));
                assert!(request.contains("x-xai-token-auth: xai-grok-cli"));
                if replace_identity {
                    assert_eq!(result.unwrap_err().to_string(), BILLING_ACCOUNT_CHANGED);
                } else if status == 200 && body == BILLING_BODY {
                    let usage = result.unwrap().unwrap();
                    assert_eq!(usage.limits[0].percentage, Some(76));
                    assert!(
                        usage
                            .limits
                            .iter()
                            .any(|limit| limit.label.contains("observed"))
                    );
                } else {
                    let error = result.unwrap_err();
                    assert_eq!(error.status(), (status != 200).then_some(status));
                }
            },
            async {
                smol::Timer::after(TEST_TIMEOUT).await;
                panic!("billing mock request timed out");
            },
        ));
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
