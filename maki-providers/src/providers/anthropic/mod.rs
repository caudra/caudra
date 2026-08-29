//! Anthropic allows 4 cache breakpoints per request. We place them on: the last tool
//! definition, the system prompt, and the last block of the 2 most recent messages.

pub mod auth;
pub(crate) mod bedrock;
pub(crate) mod shared;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
};

use super::KeyPool;

const API_VERSION: &str = "2023-06-01";
const API_ORIGIN: &str = auth::API_ORIGIN;
const MESSAGES_PATH: &str = "/v1/messages";
const OAUTH_MESSAGES_PATH: &str = "/v1/messages?beta=true";
const MODELS_PATH: &str = "/v1/models?limit=1000";
const USAGE_PATH: &str = "/api/oauth/usage";
const PROFILE_PATH: &str = "/api/oauth/profile";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const OAUTH_MESSAGE_BETAS: &[&str] = &[
    "claude-code-20250219",
    OAUTH_BETA,
    "interleaved-thinking-2025-05-14",
    "prompt-caching-scope-2026-01-05",
    "context-management-2025-06-27",
    "advisor-tool-2026-03-01",
    "thinking-token-count-2026-05-13",
    "extended-cache-ttl-2025-04-11",
];
const MONEY_EXPONENT: u32 = 2;
const LABEL_SESSION: &str = "5-hour usage";
const LABEL_WEEK_ALL: &str = "Weekly usage";
const EMPTY_USAGE_ERROR: &str =
    "Anthropic usage response contained no quota limits; the endpoint schema may have changed";
const OAUTH_METADATA_TIMEOUT: Duration = Duration::from_secs(10);

const ENV_VAR: &str = "ANTHROPIC_API_KEY";

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "anthropic",
    display_name: "Anthropic",
    protocol: maki_config::providers::Protocol::Anthropic,
    default_base_url: API_ORIGIN,
    default_api_key_env: ENV_VAR,
    default_model: "anthropic/claude-sonnet-4-6",
    plans: None,
    login_url: Some("https://console.anthropic.com/settings/keys"),
    needs_url: false,
});

pub(crate) use shared::models;

/// Returns whether the fast-mode beta header must be attached. We re-check
/// `supports_fast()` here rather than trusting `opts.fast` alone, so a stale UI
/// flag can never bill an ineligible model at the premium fast-mode rate.
fn apply_fast_mode(body: &mut Value, model: &Model, opts: RequestOptions) -> bool {
    let on = opts.fast && model.supports_fast();
    if on {
        body["speed"] = json!("fast");
    }
    on
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OauthUsage {
    limits: Option<Vec<Value>>,
    spend: Option<Spend>,
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_oauth_apps: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    cinder_cove: Option<UsageWindow>,
    extra_usage: Option<ExtraUsage>,
}

#[derive(Deserialize)]
struct ApiLimit {
    kind: String,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Spend {
    enabled: bool,
    percent: Option<f64>,
    used: Option<Money>,
}

#[derive(Deserialize)]
struct Money {
    amount_minor: i64,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    exponent: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExtraUsage {
    is_enabled: bool,
    utilization: Option<f64>,
    used_credits: Option<f64>,
    currency: Option<String>,
    decimal_places: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OauthProfile {
    account: Option<ProfileAccount>,
    organization: Option<ProfileOrganization>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProfileAccount {
    has_claude_pro: bool,
    has_claude_max: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProfileOrganization {
    organization_type: Option<String>,
}

fn parse_reset(rfc3339: &str) -> Option<u64> {
    let ts: jiff::Timestamp = rfc3339.parse().ok()?;
    u64::try_from(ts.as_millisecond()).ok()
}

fn spent(minor_units: f64, exponent: Option<u32>, currency: Option<&str>) -> String {
    let amount = minor_units / 10f64.powi(exponent.unwrap_or(MONEY_EXPONENT) as i32);
    match currency {
        None | Some("USD") => format!("${amount:.2} spent"),
        Some(c) => format!("{amount:.2} {c} spent"),
    }
}

fn usage_percentage(percentage: f64) -> Option<u32> {
    percentage
        .is_finite()
        .then(|| percentage.round().clamp(0.0, 100.0) as u32)
}

fn limit_label(l: &ApiLimit) -> String {
    match l.kind.as_str() {
        "session" => LABEL_SESSION.into(),
        "weekly_all" => LABEL_WEEK_ALL.into(),
        "weekly_scoped" => match l
            .scope
            .pointer("/model/display_name")
            .or_else(|| l.scope.pointer("/surface/display_name"))
            .and_then(Value::as_str)
        {
            Some(name) => format!("Weekly {name} usage"),
            None => LABEL_WEEK_ALL.into(),
        },
        other => other.into(),
    }
}

fn credits_limit(u: &OauthUsage) -> Option<UsageLimit> {
    if let Some(spend) = u.spend.as_ref().filter(|spend| spend.enabled) {
        let detail = spend.used.as_ref().map(|money| {
            spent(
                money.amount_minor as f64,
                money.exponent,
                money.currency.as_deref(),
            )
        });
        let percentage = spend.percent.and_then(usage_percentage);
        if percentage.is_some() || detail.is_some() {
            return Some(UsageLimit {
                label: "Usage credits".into(),
                percentage,
                reset_at: None,
                detail,
            });
        }
    }
    let extra = u.extra_usage.as_ref().filter(|extra| extra.is_enabled)?;
    let detail = extra
        .used_credits
        .map(|credits| spent(credits, extra.decimal_places, extra.currency.as_deref()));
    let percentage = extra.utilization.and_then(usage_percentage);
    (percentage.is_some() || detail.is_some()).then_some(UsageLimit {
        label: "Usage credits".into(),
        percentage,
        reset_at: None,
        detail,
    })
}

impl From<OauthUsage> for ProviderUsage {
    fn from(u: OauthUsage) -> Self {
        let mut limits = Vec::new();
        let mut has_session = false;
        let mut has_weekly = false;
        let mut has_oauth_apps = false;
        let mut has_sonnet = false;
        let mut has_opus = false;
        let mut has_cinder_cove = false;
        for value in u.limits.as_deref().unwrap_or_default() {
            let Ok(limit) = serde_json::from_value::<ApiLimit>(value.clone()) else {
                continue;
            };
            match limit.kind.as_str() {
                "session" => has_session = true,
                "weekly_all" => has_weekly = true,
                "weekly_oauth_apps" | "seven_day_oauth_apps" => has_oauth_apps = true,
                "cinder_cove" => has_cinder_cove = true,
                "weekly_scoped" => {
                    let name = limit
                        .scope
                        .pointer("/model/display_name")
                        .or_else(|| limit.scope.pointer("/surface/display_name"))
                        .and_then(Value::as_str);
                    has_sonnet |= name == Some("Sonnet");
                    has_opus |= name == Some("Opus");
                    has_oauth_apps |= name.is_some_and(|name| {
                        name.eq_ignore_ascii_case("OAuth apps")
                            || name.eq_ignore_ascii_case("Claude Code")
                    });
                }
                _ => {}
            }
            let percentage = limit.percent.and_then(usage_percentage);
            let reset_at = limit.resets_at.as_deref().and_then(parse_reset);
            if percentage.is_some() || reset_at.is_some() {
                limits.push(UsageLimit {
                    label: limit_label(&limit),
                    percentage,
                    reset_at,
                    detail: None,
                });
            }
        }
        let windows = [
            (!has_session, LABEL_SESSION, &u.five_hour),
            (!has_weekly, LABEL_WEEK_ALL, &u.seven_day),
            (
                !has_oauth_apps,
                "Weekly OAuth apps usage",
                &u.seven_day_oauth_apps,
            ),
            (!has_sonnet, "Weekly Sonnet usage", &u.seven_day_sonnet),
            (!has_opus, "Weekly Opus usage", &u.seven_day_opus),
            (!has_cinder_cove, "Cinder Cove usage", &u.cinder_cove),
        ];
        limits.extend(windows.into_iter().filter_map(|(missing, label, window)| {
            let window = missing.then_some(window)?.as_ref()?;
            let percentage = window.utilization.and_then(usage_percentage);
            let reset_at = window.resets_at.as_deref().and_then(parse_reset);
            (percentage.is_some() || reset_at.is_some()).then_some(UsageLimit {
                label: label.into(),
                percentage,
                reset_at,
                detail: None,
            })
        }));
        limits.extend(credits_limit(&u));
        ProviderUsage { plan: None, limits }
    }
}

fn parse_usage(response: &str) -> Result<ProviderUsage, AgentError> {
    let usage: ProviderUsage = serde_json::from_str::<OauthUsage>(response)?.into();
    if usage.limits.is_empty() {
        return Err(AgentError::Config {
            message: EMPTY_USAGE_ERROR.into(),
        });
    }
    Ok(usage)
}

fn profile_plan(profile: &OauthProfile) -> Option<String> {
    let organization_type = profile
        .organization
        .as_ref()
        .and_then(|organization| organization.organization_type.as_deref());
    match organization_type {
        Some("claude_pro") => Some("pro".into()),
        Some("claude_max") => Some("max".into()),
        Some("claude_team") => Some("team".into()),
        Some("claude_enterprise") => Some("enterprise".into()),
        _ if profile
            .account
            .as_ref()
            .is_some_and(|account| account.has_claude_max) =>
        {
            Some("max".into())
        }
        _ if profile
            .account
            .as_ref()
            .is_some_and(|account| account.has_claude_pro) =>
        {
            Some("pro".into())
        }
        _ => None,
    }
}

/// Reduce a `base_url` to a bare origin, tolerating a trailing `/v1/messages`
/// (which Anthropic base URLs historically included) and any query string
/// (e.g. `?beta=true` from Claude Code style endpoints).
fn origin(base_url: &str) -> &str {
    let without_query = base_url.split(['?', '#']).next().unwrap_or(base_url);
    let trimmed = without_query.trim_end_matches('/');
    trimmed.strip_suffix(MESSAGES_PATH).unwrap_or(trimmed)
}

/// True when `base_url` targets the real Anthropic API, directly or via the
/// construction-time base-URL override (so quota stays visible behind a proxy).
fn first_party(base_url: &str, configured_override: Option<&str>) -> bool {
    let target = origin(base_url);
    target
        .strip_prefix("https://")
        .and_then(|rest| rest.split(['/', ':']).next())
        == Some("api.anthropic.com")
        || configured_override.is_some_and(|configured| origin(configured) == target)
}

/// Subscription quota only exists for OAuth tokens against the real Anthropic
/// API; API keys and anthropic-protocol third-party endpoints have none.
fn usage_eligible(auth: &super::ResolvedAuth, configured_override: Option<&str>) -> bool {
    auth.headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("authorization")
            && value
                .get(..7)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "))
    }) && auth
        .base_url
        .as_deref()
        .is_none_or(|url| first_party(url, configured_override))
}

fn resolve_anthropic_base_url() -> Option<String> {
    let config = maki_config::providers::ProvidersConfig::load();
    maki_config::providers::resolve_base_url("anthropic", config.get("anthropic"))
}

fn resolve_auth_from_key(key: &str, base_url: Option<String>) -> super::ResolvedAuth {
    super::ResolvedAuth {
        base_url,
        headers: vec![("x-api-key".into(), key.to_string())],
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    ApiKey,
    ClaudeOauth,
    Injected,
}

fn resolve_native_auth(
    storage: &StateDir,
    base_url: Option<String>,
) -> Result<(super::ResolvedAuth, AuthMode, Option<KeyPool>), AgentError> {
    match auth::resolve_oauth(storage) {
        Ok(Some(resolved)) => return Ok((resolved, AuthMode::ClaudeOauth, None)),
        Ok(None) => {}
        Err(oauth_error) => match KeyPool::resolve("anthropic", ENV_VAR) {
            Ok(pool) => {
                warn!(error = %oauth_error, "Anthropic OAuth unavailable; using API key");
                let resolved = resolve_auth_from_key(pool.current(), base_url);
                return Ok((resolved, AuthMode::ApiKey, Some(pool)));
            }
            Err(_) => return Err(oauth_error),
        },
    }
    let pool = KeyPool::resolve("anthropic", ENV_VAR)?;
    let resolved = resolve_auth_from_key(pool.current(), base_url);
    Ok((resolved, AuthMode::ApiKey, Some(pool)))
}

pub struct Anthropic {
    client: HttpClient,
    auth: Arc<Mutex<super::ResolvedAuth>>,
    auth_mode: Arc<Mutex<AuthMode>>,
    key_pool: Arc<Mutex<Option<KeyPool>>>,
    storage: Option<StateDir>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
    oauth_session_id: String,
    /// Env / `providers.toml` / inventory default, resolved once at construction.
    /// Reused by key rotation / reload so they do not re-parse providers.toml.
    resolved_base_url: Option<String>,
}

impl Anthropic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let resolved_base_url = resolve_anthropic_base_url();
        let (resolved, auth_mode, pool) = resolve_native_auth(&storage, resolved_base_url.clone())?;
        if let Some(pool) = &pool {
            debug!(keys = pool.len(), "using API key authentication");
        }
        Ok(Self {
            client: super::http_client(timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            auth_mode: Arc::new(Mutex::new(auth_mode)),
            key_pool: Arc::new(Mutex::new(pool)),
            storage: Some(storage),
            system_prefix: None,
            stream_timeout: timeouts.stream,
            oauth_session_id: random_id(),
            resolved_base_url,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            auth,
            auth_mode: Arc::new(Mutex::new(AuthMode::Injected)),
            key_pool: Arc::new(Mutex::new(None)),
            storage: None,
            system_prefix: None,
            stream_timeout: timeouts.stream,
            oauth_session_id: random_id(),
            // Custom / dynamic callers own their base URL; treating it as the
            // anthropic override would make every third-party endpoint look
            // first party and poll `/api/oauth/usage` against it.
            resolved_base_url: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn is_oauth(&self) -> bool {
        *self.auth_mode.lock().unwrap() == AuthMode::ClaudeOauth
    }

    fn current_auth(&self) -> super::ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    async fn refresh_oauth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let rejected_access = bearer_token(&self.current_auth());
        let tokens =
            smol::unblock(move || auth::refresh_from_storage(&storage, rejected_access.as_deref()))
                .await?;
        *self.auth.lock().unwrap() = auth::build_oauth_resolved(&tokens);
        debug!("refreshed Anthropic OAuth token");
        Ok(())
    }

    async fn ensure_oauth_fresh(&self) {
        let Some(storage) = self.storage.clone().filter(|_| self.is_oauth()) else {
            return;
        };
        match smol::unblock(move || auth::refresh_from_storage(&storage, None)).await {
            Ok(tokens) => *self.auth.lock().unwrap() = auth::build_oauth_resolved(&tokens),
            Err(error) => warn!(%error, "proactive Anthropic OAuth refresh failed"),
        }
    }

    async fn with_oauth_retry<T, F, Fut>(&self, operation: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        self.ensure_oauth_fresh().await;
        let result = operation().await;
        if self.is_oauth()
            && matches!(&result, Err(error) if oauth_auth_error(error))
            && self.refresh_oauth().await.is_ok()
        {
            return operation().await;
        }
        result
    }

    fn build_request(&self, method: &str, path: &str) -> isahc::http::request::Builder {
        self.build_request_with_session(method, path, None)
    }

    fn build_request_with_session(
        &self,
        method: &str,
        path: &str,
        session_id: Option<&str>,
    ) -> isahc::http::request::Builder {
        let auth = self.current_auth();
        let base = auth.base_url.as_deref().unwrap_or(API_ORIGIN);
        let url = format!("{}{path}", origin(base));
        let oauth = self.is_oauth();
        let user_agent = if oauth {
            format!("claude-cli/{} (external, cli)", auth::CLAUDE_CODE_VERSION)
        } else {
            super::user_agent().into()
        };
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("anthropic-version", API_VERSION)
            .header("user-agent", user_agent);
        if oauth {
            builder = builder
                .header("x-app", "cli")
                .header(
                    "x-claude-code-session-id",
                    session_id.unwrap_or(&self.oauth_session_id),
                )
                .header("x-client-request-id", random_id())
                .header("anthropic-dangerous-direct-browser-access", "true");
        }
        auth.configure_request(builder)
    }

    async fn do_stream_request(
        &self,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
        fast: bool,
        long_context: bool,
        session_id: Option<&SessionRef>,
        oauth_tool_names: Option<&HashMap<String, String>>,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let path = if self.is_oauth() {
            OAUTH_MESSAGES_PATH
        } else {
            MESSAGES_PATH
        };
        let mut builder = self
            .build_request_with_session("POST", path, session_id.map(SessionRef::as_str))
            .header("content-type", "application/json");
        let mut betas = Vec::new();
        if self.is_oauth() {
            betas.extend_from_slice(OAUTH_MESSAGE_BETAS);
        }
        if fast {
            betas.push(FAST_MODE_BETA);
        }
        if long_context {
            betas.push(shared::LONG_CONTEXT_BETA);
        }
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }
        let request = builder.body(json_body)?;
        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse_inner(response, event_tx, self.stream_timeout, oauth_tool_names).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn do_list_models(&self) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut path = MODELS_PATH.to_string();
            if let Some(cursor) = &after_id {
                path.push_str(&format!("&after_id={cursor}"));
            }

            let request = self.build_request("GET", &path).body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }

            let body_text = response.text().await?;
            let page: ModelsPage = serde_json::from_str(&body_text)?;
            for m in page.data {
                if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                    models.push(crate::model::ModelInfo::id_only(format!(
                        "{}{}",
                        m.id,
                        shared::LONG_CONTEXT_SUFFIX
                    )));
                }
                models.push(crate::model::ModelInfo::id_only(m.id));
            }

            if !page.has_more {
                break;
            }
            after_id = page.last_id;
        }

        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn do_oauth_get(&self, path: &str) -> Result<String, AgentError> {
        let request = self
            .build_request("GET", path)
            .timeout(OAUTH_METADATA_TIMEOUT)
            .header("anthropic-beta", OAUTH_BETA)
            .header("content-type", "application/json")
            .body(())?;
        let mut response = self.client.send_async(request).await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    }

    async fn do_fetch_usage(&self) -> Result<ProviderUsage, AgentError> {
        parse_usage(&self.do_oauth_get(USAGE_PATH).await?)
    }

    async fn do_fetch_profile_plan(&self) -> Result<Option<String>, AgentError> {
        let profile: OauthProfile = serde_json::from_str(&self.do_oauth_get(PROFILE_PATH).await?)?;
        Ok(profile_plan(&profile))
    }
}

fn bearer_token(auth: &super::ResolvedAuth) -> Option<String> {
    auth.headers.iter().find_map(|(key, value)| {
        key.eq_ignore_ascii_case("authorization")
            .then(|| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            })
            .flatten()
            .map(ToOwned::to_owned)
    })
}

fn oauth_auth_error(error: &AgentError) -> bool {
    matches!(error, AgentError::Api { status: 401, .. })
        || matches!(error, AgentError::Api { status: 403, message } if message.contains("OAuth token has been revoked"))
}

fn random_id() -> String {
    let high = fastrand::u64(..);
    let low = fastrand::u64(..);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0xffff,
        low >> 48,
        low & 0xffff_ffff_ffff,
    )
}

impl Provider for Anthropic {
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
            let system_blocks = if let Some(prefix) = &self.system_prefix {
                vec![
                    shared::SystemBlock {
                        r#type: "text",
                        text: prefix,
                        cache_control: None,
                    },
                    shared::SystemBlock {
                        r#type: "text",
                        text: system,
                        cache_control: Some(shared::EPHEMERAL),
                    },
                ]
            } else {
                vec![shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }]
            };

            let mut body = shared::build_request_body_with_system(
                model,
                messages,
                &system_blocks,
                tools,
                opts.thinking,
            );
            body["model"] = json!(shared::strip_long_context(&model.id));
            body["stream"] = json!(true);
            let oauth_tool_names = if self.is_oauth() {
                Some(shared::apply_oauth_request_profile(
                    &mut body,
                    system,
                    auth::CLAUDE_CODE_VERSION,
                ))
            } else {
                None
            };
            let fast = apply_fast_mode(&mut body, model, opts);
            let long_context = model.id.ends_with(shared::LONG_CONTEXT_SUFFIX);

            debug!(model = %model.id, num_messages = messages.len(), thinking = ?opts.thinking, fast, long_context, "sending API request");
            if !self.is_oauth() {
                return self
                    .do_stream_request(&body, event_tx, fast, long_context, session_id, None)
                    .await;
            }

            self.ensure_oauth_fresh().await;
            let (relay_tx, relay_rx) = flume::unbounded();
            let attempt = async {
                let result = self
                    .do_stream_request(
                        &body,
                        &relay_tx,
                        fast,
                        long_context,
                        session_id,
                        oauth_tool_names.as_ref(),
                    )
                    .await;
                drop(relay_tx);
                result
            };
            let forward = async move {
                let mut forwarded = 0usize;
                while let Ok(event) = relay_rx.recv_async().await {
                    forwarded += 1;
                    if event_tx.send_async(event).await.is_err() {
                        break;
                    }
                }
                forwarded
            };
            let (result, forwarded) = futures_lite::future::zip(attempt, forward).await;
            if matches!(&result, Err(error) if oauth_auth_error(error))
                && forwarded == 0
                && self.refresh_oauth().await.is_ok()
            {
                return self
                    .do_stream_request(
                        &body,
                        event_tx,
                        fast,
                        long_context,
                        session_id,
                        oauth_tool_names.as_ref(),
                    )
                    .await;
            }
            result
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async { self.with_oauth_retry(|| self.do_list_models()).await })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(storage) = self.storage.clone() else {
                return Ok(());
            };
            let base_url = self.resolved_base_url.clone();
            let (resolved, mode, pool) =
                smol::unblock(move || resolve_native_auth(&storage, base_url)).await?;
            *self.auth.lock().unwrap() = resolved;
            *self.auth_mode.lock().unwrap() = mode;
            *self.key_pool.lock().unwrap() = pool;
            debug!("reloaded Anthropic auth from storage and environment");
            Ok(())
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                self.refresh_oauth().await
            } else {
                self.reload_auth().await
            }
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            let base_url = self.resolved_base_url.clone();
            Ok(self.key_pool.lock().unwrap().as_ref().is_some_and(|p| {
                p.rotate_auth(&self.auth, |key| {
                    resolve_auth_from_key(key, base_url.clone())
                })
            }))
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            if !usage_eligible(&self.current_auth(), self.resolved_base_url.as_deref()) {
                return Ok(None);
            }
            let mut usage = self.with_oauth_retry(|| self.do_fetch_usage()).await?;
            match self.with_oauth_retry(|| self.do_fetch_profile_plan()).await {
                Ok(plan) => usage.plan = plan,
                Err(error) => warn!(%error, "failed to fetch Anthropic OAuth profile"),
            }
            Ok(Some(usage))
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        if self.is_oauth() {
            model.pricing.input = 0.0;
            model.pricing.output = 0.0;
            model.pricing.cache_write = 0.0;
            model.pricing.cache_read = 0.0;
        }
    }
}

#[derive(Deserialize)]
struct ApiModelInfo {
    id: String,
    #[serde(default)]
    max_input_tokens: u32,
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<ApiModelInfo>,
    has_more: bool,
    last_id: Option<String>,
}

pub(crate) async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    parse_sse_inner(response, event_tx, stream_timeout, None).await
}

async fn parse_sse_inner(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
    oauth_tool_names: Option<&HashMap<String, String>>,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    let mut parser = oauth_tool_names.map_or_else(shared::EventParser::new, |names| {
        shared::EventParser::new_oauth(names.clone())
    });
    let mut current_event = String::new();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(rest) = line.strip_prefix("event:") {
            current_event = rest.strip_prefix(' ').unwrap_or(rest).to_string();
            continue;
        }

        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            None => continue,
        };

        if parser
            .process(&current_event, data, event_tx)
            .await?
            .is_break()
        {
            break;
        }
    }

    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContentBlock, EMPTY_RESPONSE_MARKER, MakiId, ProviderEvent, Role, StopReason, TokenUsage,
    };
    use maki_storage::tool_outputs::ToolOutputRef;
    use serde_json::{Value, json};
    use shared::build_wire_messages;
    use std::time::Duration;
    use test_case::test_case;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    const THIRD_PARTY_BASE_URL: &str = "https://proxy.example.com/v1/messages";
    const STORED_OUTPUT: &str = "first\nsecond";

    const USAGE_BODY: &str = r#"{
        "five_hour": {"utilization": 14.0, "resets_at": "2026-02-06T22:00:00+00:00"},
        "seven_day": {"utilization": 2.0,  "resets_at": "2026-02-09T00:00:00+00:00"},
        "limits": [
            {"kind": "session",       "group": "session", "percent": 14, "severity": "normal",
             "resets_at": "2026-02-06T22:00:00+00:00", "scope": null, "is_active": true},
            {"kind": "weekly_all",    "group": "weekly",  "percent": 2,  "severity": "normal",
             "resets_at": "2026-02-09T00:00:00+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly",  "percent": 3,  "severity": "normal",
             "resets_at": "2026-02-09T00:00:00+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
             "is_active": false}
        ],
        "extra_usage": {"is_enabled": true, "monthly_limit": 15000, "used_credits": 233.0,
                        "utilization": 1.55, "currency": "USD", "decimal_places": 2},
        "spend": {
            "used":  {"amount_minor": 233,   "currency": "USD", "exponent": 2},
            "limit": {"amount_minor": 15000, "currency": "USD", "exponent": 2},
            "percent": 2, "severity": "normal", "enabled": true
        }
    }"#;

    #[test]
    fn parse_oauth_usage_response() {
        let parsed: OauthUsage = serde_json::from_str(USAGE_BODY).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert!(usage.plan.is_none());
        assert_eq!(usage.limits.len(), 4);
        assert_eq!(usage.limits[0].label, "5-hour usage");
        assert_eq!(usage.limits[0].percentage, Some(14));
        assert_eq!(usage.limits[0].reset_at, Some(1770415200000));
        assert_eq!(usage.limits[1].label, "Weekly usage");
        assert_eq!(usage.limits[1].percentage, Some(2));
        assert_eq!(usage.limits[2].label, "Weekly Fable usage");
        assert_eq!(usage.limits[2].percentage, Some(3));
        assert_eq!(usage.limits[3].label, "Usage credits");
        assert_eq!(usage.limits[3].percentage, Some(2));
        assert_eq!(usage.limits[3].reset_at, None);
        assert_eq!(usage.limits[3].detail.as_deref(), Some("$2.33 spent"));
    }

    #[test]
    fn parse_oauth_usage_windows_fallback() {
        let body = r#"{
            "five_hour":        {"utilization": 35.4, "resets_at": "2026-02-06T22:00:00+00:00"},
            "seven_day":        {"utilization": 14.0, "resets_at": "2026-02-09T00:00:00+00:00"},
            "seven_day_sonnet": {"utilization": 39.0, "resets_at": "2026-02-09T00:00:00+00:00"},
            "seven_day_opus":   {"utilization": 2.6,  "resets_at": "2026-02-09T00:00:00+00:00"},
            "extra_usage":      {"is_enabled": true, "used_credits": 233.0, "utilization": 2.0}
        }"#;
        let parsed: OauthUsage = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.limits.len(), 5);
        assert_eq!(usage.limits[0].label, "5-hour usage");
        assert_eq!(usage.limits[0].percentage, Some(35));
        assert_eq!(usage.limits[0].reset_at, Some(1770415200000));
        assert_eq!(usage.limits[1].label, "Weekly usage");
        assert_eq!(usage.limits[2].label, "Weekly Sonnet usage");
        assert_eq!(usage.limits[3].label, "Weekly Opus usage");
        assert_eq!(usage.limits[3].percentage, Some(3));
        assert_eq!(usage.limits[4].label, "Usage credits");
        assert_eq!(usage.limits[4].percentage, Some(2));
        assert_eq!(usage.limits[4].detail.as_deref(), Some("$2.33 spent"));
    }

    #[test]
    fn parse_oauth_usage_null_windows_skipped() {
        let body = r#"{
            "five_hour": {"utilization": 5.0, "resets_at": "not a timestamp"},
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": true, "utilization": null}
        }"#;
        let parsed: OauthUsage = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].label, "5-hour usage");
        assert_eq!(usage.limits[0].reset_at, None);
    }

    #[test]
    fn structured_limits_fill_missing_flat_windows() {
        let body = r#"{
            "limits": [{"kind": "weekly_all", "percent": 24, "resets_at": null}],
            "five_hour": {"utilization": 12, "resets_at": "2026-02-06T22:00:00Z"}
        }"#;
        let usage = parse_usage(body).unwrap();
        assert_eq!(usage.limits.len(), 2);
        assert_eq!(usage.limits[0].label, "Weekly usage");
        assert_eq!(usage.limits[1].label, "5-hour usage");
    }

    #[test]
    fn null_and_malformed_limits_fall_back_to_windows() {
        let null_limits = r#"{"limits": null, "seven_day": {"utilization": 8}}"#;
        assert_eq!(
            parse_usage(null_limits).unwrap().limits[0].label,
            "Weekly usage"
        );

        let malformed = r#"{
            "limits": [{"percent": "bad"}, {"kind": "session", "percent": 150}],
            "seven_day": {"utilization": -4}
        }"#;
        let usage = parse_usage(malformed).unwrap();
        assert_eq!(usage.limits[0].percentage, Some(100));
        assert_eq!(usage.limits[1].percentage, Some(0));
    }

    #[test]
    fn current_special_usage_windows_are_preserved() {
        let usage = parse_usage(
            r#"{
                "seven_day_oauth_apps": {"utilization": 18},
                "cinder_cove": {"utilization": 7}
            }"#,
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 2);
        assert_eq!(usage.limits[0].label, "Weekly OAuth apps usage");
        assert_eq!(usage.limits[1].label, "Cinder Cove usage");
    }

    #[test]
    fn empty_usage_response_is_rejected() {
        assert_eq!(
            parse_usage("{}").unwrap_err().to_string(),
            EMPTY_USAGE_ERROR
        );
    }

    #[test_case("claude_pro", "pro")]
    #[test_case("claude_max", "max")]
    #[test_case("claude_team", "team")]
    #[test_case("claude_enterprise", "enterprise")]
    fn profile_maps_subscription_type(organization_type: &str, expected: &str) {
        let profile = OauthProfile {
            organization: Some(ProfileOrganization {
                organization_type: Some(organization_type.into()),
            }),
            ..Default::default()
        };
        assert_eq!(profile_plan(&profile).as_deref(), Some(expected));
    }

    #[test]
    fn oauth_request_profile_rewrites_identity_and_tools() {
        let mut body = json!({
            "system": [{"type": "text", "text": "old"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello world"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "1", "name": "bash", "input": {}}]}
            ],
            "tools": [
                {"name": "bash", "input_schema": {"type": "object"}},
                {"name": "mcp_fetch", "input_schema": {"type": "object"}},
                {"name": "Bash", "input_schema": {"type": "object"}},
                {"name": "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl", "input_schema": {"type": "object"}}
            ]
        });
        let tool_names = shared::apply_oauth_request_profile(&mut body, "Maki system", "2.1.248");
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("x-anthropic-billing-header:")
        );
        assert_eq!(
            body["system"][1]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(body["messages"][0]["content"][0]["text"], "Maki system");
        assert_eq!(body["tools"][0]["name"], "mcp_Bash");
        assert_eq!(body["tools"][1]["name"], "mcp_Mcp_fetch");
        assert_eq!(tool_names["mcp_Mcp_fetch"], "mcp_fetch");
        assert_ne!(body["tools"][2]["name"], body["tools"][0]["name"]);
        assert!(body["tools"][3]["name"].as_str().unwrap().len() <= 64);
        assert_eq!(body["messages"][1]["content"][0]["name"], "mcp_Bash");
    }

    #[test]
    fn oauth_request_uses_first_party_headers_once() {
        let provider = Anthropic::with_auth(
            Arc::new(Mutex::new(auth::build_oauth_resolved(
                &maki_storage::auth::OAuthTokens {
                    access: "token".into(),
                    refresh: "refresh".into(),
                    expires: u64::MAX,
                    account_id: None,
                },
            ))),
            crate::providers::Timeouts::default(),
        );
        *provider.auth_mode.lock().unwrap() = AuthMode::ClaudeOauth;
        let request = provider
            .build_request_with_session("POST", OAUTH_MESSAGES_PATH, Some("session-id"))
            .body(())
            .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer token"
        );
        assert_eq!(request.headers().get("x-app").unwrap(), "cli");
        assert_eq!(
            request.headers().get("x-claude-code-session-id").unwrap(),
            "session-id"
        );
        assert_eq!(request.headers().get_all("user-agent").iter().count(), 1);
        assert!(request.headers().get("x-api-key").is_none());
    }

    #[test_case("Authorization", "Bearer token", None, true ; "bearer_default_url_eligible")]
    #[test_case("authorization", "bearer token", Some("https://api.anthropic.com/v1/messages"), true ; "bearer_anthropic_url_eligible")]
    #[test_case("authorization", "Bearer token", Some("https://api.anthropic.com"), true ; "bearer_anthropic_origin_eligible")]
    #[test_case("authorization", "Basic token", None, false ; "basic_auth_not_eligible")]
    #[test_case("x-api-key", "token", None, false ; "api_key_not_eligible")]
    #[test_case("Authorization", "Bearer token", Some("https://proxy.example.com/v1/messages"), false ; "foreign_base_url_not_eligible")]
    #[test_case("Authorization", "Bearer token", Some("https://api.anthropic.com.evil.example"), false ; "lookalike_host_not_eligible")]
    fn usage_eligibility(header: &str, value: &str, base_url: Option<&str>, expected: bool) {
        let auth = crate::providers::ResolvedAuth {
            base_url: base_url.map(String::from),
            headers: vec![(header.into(), value.into())],
        };
        assert_eq!(usage_eligible(&auth, None), expected);
    }

    #[test]
    fn with_auth_keeps_third_party_endpoint_ineligible() {
        let mut auth = crate::providers::ResolvedAuth::bearer("token");
        auth.base_url = Some(THIRD_PARTY_BASE_URL.into());
        let provider = Anthropic::with_auth(
            Arc::new(Mutex::new(auth)),
            crate::providers::Timeouts::default(),
        );
        assert!(provider.resolved_base_url.is_none());
        assert!(!usage_eligible(
            &provider.auth.lock().unwrap(),
            provider.resolved_base_url.as_deref()
        ));
    }

    #[test]
    fn usage_eligible_when_url_matches_configured_override() {
        let auth = crate::providers::ResolvedAuth {
            base_url: Some(THIRD_PARTY_BASE_URL.into()),
            headers: vec![("Authorization".into(), "Bearer token".into())],
        };
        assert!(usage_eligible(&auth, Some(THIRD_PARTY_BASE_URL)));
        assert!(!usage_eligible(
            &auth,
            Some("https://other-proxy.example.com")
        ));
    }

    #[test_case("https://api.anthropic.com/v1/messages", "https://api.anthropic.com" ; "strips_messages_path")]
    #[test_case("https://proxy.example.com/v1/messages/", "https://proxy.example.com" ; "strips_messages_path_with_trailing_slash")]
    #[test_case("https://api.anthropic.com", "https://api.anthropic.com" ; "origin_unchanged")]
    #[test_case("http://localhost:8080/", "http://localhost:8080" ; "trims_trailing_slash")]
    #[test_case("https://api.anthropic.com/v1/messages?beta=true", "https://api.anthropic.com" ; "strips_query_string")]
    fn origin_normalizes(input: &str, expected: &str) {
        assert_eq!(origin(input), expected);
    }

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(
                resp.usage,
                TokenUsage {
                    input: 42,
                    output: 10,
                    cache_creation: 5,
                    cache_read: 8
                }
            );
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text: t } = e {
                    deltas.push(t);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_no_space_after_colon() {
        smol::block_on(async {
            let sse_data = b"\
event:message_start\n\
data:{\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\
\n\
event:content_block_start\n\
data:{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event:content_block_delta\n\
data:{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\
\n\
event:message_delta\n\
data:{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\
\n\
event:message_stop\n\
data:{\"type\":\"message_stop\"}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "OK")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert_eq!(resp.usage.input, 7);
            assert_eq!(resp.usage.output, 1);
        })
    }

    #[test]
    fn parse_sse_tool_use() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "tu_1");
            assert_eq!(tools[0].1, "bash");

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("tu_1".to_string(), "bash".to_string())]);
        })
    }

    #[test]
    fn parse_oauth_sse_restores_tool_name() {
        smol::block_on(async {
            let sse_data = b"event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"mcp_Bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n";
            let (tx, rx) = flume::unbounded();
            let tool_names = HashMap::from([("mcp_Bash".into(), "bash".into())]);
            let response = parse_sse_inner(
                mock_response(sse_data),
                &tx,
                TEST_STREAM_TIMEOUT,
                Some(&tool_names),
            )
            .await
            .unwrap();
            assert_eq!(response.message.tool_uses().next().unwrap().1, "bash");
            assert!(matches!(
                rx.recv().unwrap(),
                ProviderEvent::ToolUseStart { name, .. } if name == "bash"
            ));
        });
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn thinking_block(thinking: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: thinking.into(),
            signature: None,
        }
    }

    fn output_ref() -> ToolOutputRef {
        ToolOutputRef {
            id: MakiId::generate().to_string().parse().unwrap(),
            byte_count: STORED_OUTPUT.len(),
            line_count: STORED_OUTPUT.lines().count(),
        }
    }

    fn message(role: Role, content: Vec<ContentBlock>) -> Message {
        Message {
            role,
            content,
            ..Default::default()
        }
    }

    /// `expected` names the (message, block) pairs that should carry a breakpoint.
    #[test_case(vec![Message::user("only".into())], &[(0, 0)] ; "single_message")]
    #[test_case(
        vec![
            Message::user("first".into()),
            message(Role::Assistant, vec![text_block("reply")]),
            message(Role::User, vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                    output_ref: None,
                },
                text_block("second"),
            ]),
        ],
        &[(1, 0), (2, 1)]
        ; "last_two_messages_only"
    )]
    #[test_case(
        vec![
            message(Role::Assistant, vec![
                thinking_block("hmm"),
                text_block("reply"),
                thinking_block("more"),
            ]),
            message(Role::Assistant, vec![thinking_block("stalled")]),
        ],
        &[(0, 1)]
        ; "skips_thinking_blocks"
    )]
    fn cache_control_placement(messages: Vec<Message>, expected: &[(usize, usize)]) {
        let json: Value = serde_json::to_value(build_wire_messages(&messages)).unwrap();

        let marked: Vec<(usize, usize)> = json
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .flat_map(|(msg_idx, msg)| {
                msg["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| block["cache_control"] == json!({"type": "ephemeral"}))
                    .map(move |(block_idx, _)| (msg_idx, block_idx))
            })
            .collect();
        assert_eq!(marked, expected);
    }

    #[test]
    fn blank_text_blocks_never_reach_the_wire() {
        let messages = vec![
            message(Role::Assistant, vec![text_block(" \n"), text_block("kept")]),
            message(Role::Assistant, vec![text_block("   ")]),
        ];
        let json: Value = serde_json::to_value(build_wire_messages(&messages)).unwrap();

        assert_eq!(json[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(json[0]["content"][0]["text"], "kept");
        assert_eq!(json[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(json[1]["content"][0]["text"], EMPTY_RESPONSE_MARKER);
    }

    #[test]
    fn tool_result_with_trailing_image_serializes_valid_wire_blocks() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                    output_ref: Some(output_ref()),
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Png,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let wire = build_wire_messages(&messages);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(
            json[0]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": "[image: pic.png 1KB]",
            })
        );
        assert!(json[0]["content"][0].get("output_ref").is_none());
        assert_eq!(
            json[0]["content"][1],
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "aGVsbG8=",
                },
                "cache_control": {"type": "ephemeral"},
            })
        );
    }

    #[test]
    fn apply_fast_mode_sets_speed_on_capable_model() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(header);
        assert_eq!(body["speed"], json!("fast"));
    }

    #[test]
    fn apply_fast_mode_ignores_stale_flag_on_ineligible_model() {
        // Sonnet is not fast-capable, so opts.fast=true must still skip `speed`.
        let model = Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn apply_fast_mode_off_when_not_requested() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(&mut body, &model, RequestOptions::default());
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn long_context_spec_resolves_to_1m_window() {
        let model = Model::from_spec("anthropic/claude-opus-4-8-1m").unwrap();
        assert_eq!(model.id, "claude-opus-4-8-1m");
        assert_eq!(model.context_window, shared::LONG_CONTEXT_WINDOW);
        assert!(model.id.ends_with(shared::LONG_CONTEXT_SUFFIX));
        // The API has never heard of `-1m`, so strip it before sending.
        assert_eq!(shared::strip_long_context(&model.id), "claude-opus-4-8");
    }

    #[test]
    fn list_models_adds_1m_variant_from_max_input_tokens() {
        // The real /v1/models payload hides the 1M window in `max_input_tokens`.
        let page: ModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {"id": "claude-opus-4-8", "max_input_tokens": 1000000},
                    {"id": "claude-opus-4-5-20251101", "max_input_tokens": 200000}
                ],
                "has_more": false,
                "last_id": null
            }"#,
        )
        .unwrap();

        let mut models = Vec::new();
        for m in page.data {
            if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                models.push(format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX));
            }
            models.push(m.id);
        }
        models.sort();

        assert_eq!(
            models,
            vec![
                "claude-opus-4-5-20251101".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-8-1m".to_string(),
            ]
        );
    }

    #[test]
    fn parse_sse_overloaded_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_unparseable_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: not-json\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 400);
                    assert_eq!(message, "not-json");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_yields_empty_object() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_2\",\"name\":\"read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{broken\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(*tools[0].2, Value::Object(Default::default()));
        })
    }

    #[test]
    fn parse_sse_thinking_blocks() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, signature }
                    if thinking == "Let me think" && *signature == Some("sig123".to_string()))
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let thinking_deltas: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me", " think"]);
        })
    }

    #[test]
    fn parse_sse_redacted_thinking() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_data\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::RedactedThinking { data } if data == "opaque_data")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hi")
            );
        })
    }
}
