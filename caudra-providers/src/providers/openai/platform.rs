use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caudra_storage::StateDir;
use caudra_storage::auth::OAuthTokens;
use caudra_storage::log::{outcome, target};
use flume::Sender;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::model::{Billing, Model, ModelInfo};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    UsageLimit,
};

use super::auth;
use super::responses::apply_prompt_cache_key;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, catalog};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "openai",
    api_key_env: "OPENAI_API_KEY",
    base_url: "https://api.openai.com/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: true,
    provider_name: "OpenAI",
};

// Non-codex models OpenAI offers for subscription usage via the Coding Plan.
// Codex models are matched by their `-codex` substring in
// `coding_plan_context_window`, so they never need listing here.
pub(crate) const PLAN_MODELS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
];

/// Families that accept an explicit `prompt_cache_breakpoint`; earlier models
/// are implicit-only and reject the field.
const EXPLICIT_CACHE_FAMILIES: &[&str] = &["gpt-5.6-", "gpt-6-"];
const CODEX_PLAN_CONTEXT_WINDOW: u32 = 272_000;
/// Plan window for the long-context families, gpt-5.6 and gpt-astra. Neither is
/// published; `adjust_model` clamps to the smaller of this and the model's own
/// window, so this only ever narrows what the static table already declared.
const WIDE_PLAN_CONTEXT_WINDOW: u32 = 372_000;
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// The header the Codex backend derives cache affinity from; Codex CLI sends
/// its thread id here alongside the body's `prompt_cache_key`.
const CODEX_SESSION_HEADER: &str = "session-id";
/// How OpenAI spells fast mode: a service tier on the request body, where
/// Anthropic uses a `speed` field and a beta header.
const PRIORITY_SERVICE_TIER: &str = "priority";
const EMPTY_USAGE_ERROR: &str =
    "OpenAI usage response contained no plan or rate limits; the endpoint schema likely changed";
const MILLIS_PER_SECOND: u64 = 1_000;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

#[derive(Clone, PartialEq, Eq)]
struct AuthSnapshot {
    resolved: ResolvedAuth,
    oauth_tokens: Option<OAuthTokens>,
}

struct AuthState {
    resolved: Arc<Mutex<ResolvedAuth>>,
    oauth_tokens: Option<OAuthTokens>,
}

fn is_codex_model(model_id: &str) -> bool {
    coding_plan_context_window(model_id).is_some()
}

/// Asks for the priority tier when fast mode is on. `supports_fast()` is
/// re-checked here rather than trusting `opts.fast` alone, so a stale UI flag
/// can never bill an ineligible model at the premium rate.
fn apply_fast_mode(body: &mut Value, model: &Model, opts: &RequestOptions) {
    if opts.fast && model.supports_fast() {
        body["service_tier"] = json!(PRIORITY_SERVICE_TIER);
    }
}

fn supports_explicit_cache(model_id: &str) -> bool {
    EXPLICIT_CACHE_FAMILIES
        .iter()
        .any(|family| model_id.starts_with(family))
}

// Codex models match by substring so future releases route without a registry
// edit; the named non-codex plans match exactly to avoid catching near-misses
// like `gpt-5.6-terra-preview`.
fn coding_plan_context_window(model_id: &str) -> Option<u32> {
    if model_id.contains("-codex") {
        return Some(CODEX_PLAN_CONTEXT_WINDOW);
    }
    if !PLAN_MODELS.contains(&model_id) {
        return None;
    }
    Some(
        if model_id.starts_with("gpt-5.6-") || model_id.starts_with("gpt-6-") {
            WIDE_PLAN_CONTEXT_WINDOW
        } else {
            CODEX_PLAN_CONTEXT_WINDOW
        },
    )
}

/// Models an OAuth session may run. The subscription sells a different set to
/// the metered API, and models.dev describes only the latter, so the catalog can
/// widen the candidates but never decide entitlement: `is_codex_model` does.
/// Drawing on it anyway means a new `-codex` release needs no edit here.
fn coding_plan_models() -> Vec<ModelInfo> {
    let statics = super::models()
        .iter()
        .flat_map(|e| e.prefixes.iter().copied());
    let catalog = catalog::model_ids_if_available(CONFIG.slug);
    let mut ids: Vec<String> = statics
        .map(str::to_string)
        .chain(catalog)
        .filter(|id| is_codex_model(id))
        .collect();
    ids.sort();
    ids.dedup();
    ids.into_iter().map(ModelInfo::id_only).collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexUsage {
    plan_type: Option<String>,
    rate_limit: CodexRateLimit,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexRateLimit {
    primary_window: Option<CodexUsageWindow>,
    secondary_window: Option<CodexUsageWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexUsageWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<u64>,
}

pub struct OpenAi {
    compat: OpenAiCompatProvider,
    auth_state: Mutex<AuthState>,
    auth_update: async_lock::Mutex<()>,
    rejected_auth_credentials: Mutex<HashSet<String>>,
    model_context_windows: Mutex<HashMap<String, u32>>,
    storage: Option<StateDir>,
    system_prefix: Option<String>,
    /// Env / `providers.toml` override for the platform API, resolved once at
    /// construction. Used by the Responses (codex) path only; ChatGPT Coding
    /// Plan OAuth keeps its fixed backend URL.
    resolved_base_url: Option<String>,
}

impl OpenAi {
    pub fn new(timeouts: crate::providers::Timeouts) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let (resolved, oauth_tokens) = auth::resolve_with_tokens(&storage)?;
        let compat = OpenAiCompatProvider::new(&CONFIG, timeouts);
        Ok(Self {
            resolved_base_url: resolve_openai_base_url(),
            compat,
            auth_state: Mutex::new(AuthState {
                resolved: Arc::new(Mutex::new(resolved)),
                oauth_tokens,
            }),
            auth_update: async_lock::Mutex::new(()),
            rejected_auth_credentials: Mutex::new(HashSet::new()),
            model_context_windows: Mutex::new(HashMap::new()),
            storage: Some(storage),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: crate::providers::Timeouts,
    ) -> Self {
        Self {
            resolved_base_url: resolve_openai_base_url(),
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth_state: Mutex::new(AuthState {
                resolved: auth,
                oauth_tokens: None,
            }),
            auth_update: async_lock::Mutex::new(()),
            rejected_auth_credentials: Mutex::new(HashSet::new()),
            model_context_windows: Mutex::new(HashMap::new()),
            storage: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn auth_snapshot(&self) -> AuthSnapshot {
        let state = self.auth_state.lock().unwrap();
        let resolved = state.resolved.lock().unwrap().clone();
        AuthSnapshot {
            resolved,
            oauth_tokens: state.oauth_tokens.clone(),
        }
    }

    fn is_oauth(&self) -> bool {
        self.auth_snapshot().oauth_tokens.is_some()
    }

    fn rejected_auth_credentials(&self) -> Vec<String> {
        self.rejected_auth_credentials
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    fn mark_auth_rejected(&self, credential: Option<String>) {
        if let Some(credential) = credential {
            self.rejected_auth_credentials
                .lock()
                .unwrap()
                .insert(credential);
        }
    }

    fn auth_replacement_available(&self, snapshot: &AuthSnapshot) -> bool {
        bearer_token(&snapshot.resolved).is_some_and(|access| {
            !self
                .rejected_auth_credentials
                .lock()
                .unwrap()
                .contains(&access)
        })
    }

    fn install_oauth_tokens(&self, tokens: &OAuthTokens) {
        let mut state = self.auth_state.lock().unwrap();
        *state.resolved.lock().unwrap() = auth::build_oauth_resolved(tokens);
        state.oauth_tokens = Some(tokens.clone());
        drop(state);
        let mut rejected = self.rejected_auth_credentials.lock().unwrap();
        if !rejected.contains(&tokens.access) {
            rejected.clear();
        }
    }

    fn install_resolved_auth(&self, resolved: ResolvedAuth, oauth_tokens: Option<OAuthTokens>) {
        let mut state = self.auth_state.lock().unwrap();
        *state.resolved.lock().unwrap() = resolved;
        state.oauth_tokens = oauth_tokens;
    }

    fn request_auth(&self, snapshot: &AuthSnapshot, coding_plan: bool) -> ResolvedAuth {
        if !coding_plan {
            return snapshot.resolved.clone();
        }
        if let Some(tokens) = &snapshot.oauth_tokens {
            return auth::build_coding_plan_resolved(tokens);
        }
        let mut resolved = snapshot.resolved.clone();
        if resolved.base_url.is_none() {
            resolved.base_url = self
                .resolved_base_url
                .clone()
                .or_else(|| Some(CONFIG.base_url.into()));
        }
        resolved
    }

    async fn refresh_oauth(
        &self,
        mut rejected_accesses: Vec<String>,
    ) -> Result<AuthSnapshot, AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let _update = self.auth_update.lock().await;
        let current = self.auth_snapshot();
        if current.oauth_tokens.is_none() {
            return Ok(current);
        }
        for access in self.rejected_auth_credentials() {
            if !rejected_accesses.contains(&access) {
                rejected_accesses.push(access);
            }
        }
        let tokens =
            smol::unblock(move || auth::refresh_from_storage(&storage, &rejected_accesses)).await?;
        self.install_oauth_tokens(&tokens);
        info!(
            target: target::PROVIDER,
            event = crate::auth_events::REFRESHED,
            provider = "openai",
            outcome = outcome::OK,
            "refreshed OAuth token"
        );
        Ok(self.auth_snapshot())
    }

    async fn auth_for_request(&self) -> Result<AuthSnapshot, AgentError> {
        let snapshot = self.auth_snapshot();
        let Some(storage) = self
            .storage
            .clone()
            .filter(|_| snapshot.oauth_tokens.is_some())
        else {
            return Ok(snapshot);
        };
        let _update = self.auth_update.lock().await;
        let current = self.auth_snapshot();
        if current.oauth_tokens.is_none() {
            return Ok(current);
        }
        let attempted_access = bearer_token(&current.resolved);
        let rejected_accesses = self.rejected_auth_credentials();
        let tokens =
            smol::unblock(move || auth::refresh_from_storage(&storage, &rejected_accesses))
                .await
                .inspect_err(|error| {
                    if error.is_auth_error() {
                        self.mark_auth_rejected(attempted_access);
                    }
                    warn!(%error, "proactive OpenAI OAuth refresh failed");
                })?;
        self.install_oauth_tokens(&tokens);
        Ok(self.auth_snapshot())
    }

    async fn with_oauth_retry<T, F, Fut>(
        &self,
        coding_plan: bool,
        operation: F,
    ) -> Result<T, AgentError>
    where
        F: Fn(ResolvedAuth) -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let snapshot = self.auth_for_request().await?;
        let oauth = snapshot.oauth_tokens.is_some();
        let request_auth = self.request_auth(&snapshot, coding_plan);
        let attempted_access = bearer_token(&request_auth);
        let result = operation(request_auth).await;
        let Err(error) = result else {
            return result;
        };
        if error.is_auth_error() {
            self.mark_auth_rejected(attempted_access.clone());
        }
        if !oauth || !error.is_auth_error() {
            return Err(error);
        }
        let retry_snapshot = self.refresh_oauth(self.rejected_auth_credentials()).await?;
        let retry_auth = self.request_auth(&retry_snapshot, coding_plan);
        let retry_access = bearer_token(&retry_auth);
        let retry = operation(retry_auth).await;
        if matches!(&retry, Err(error) if error.is_auth_error()) {
            self.mark_auth_rejected(retry_access);
        }
        retry
    }

    /// `operation` gets each attempt's auth, whether that auth is a login, and
    /// where to send its events.
    async fn with_oauth_stream_retry<F, Fut>(
        &self,
        coding_plan: bool,
        event_tx: &Sender<ProviderEvent>,
        operation: F,
    ) -> Result<StreamResponse, AgentError>
    where
        F: Fn(ResolvedAuth, bool, Sender<ProviderEvent>) -> Fut,
        Fut: std::future::Future<Output = Result<StreamResponse, AgentError>>,
    {
        let snapshot = self.auth_for_request().await?;
        let oauth = snapshot.oauth_tokens.is_some();
        let request_auth = self.request_auth(&snapshot, coding_plan);
        let attempted_access = bearer_token(&request_auth);
        if !oauth {
            let result = operation(request_auth, oauth, event_tx.clone()).await;
            if matches!(&result, Err(error) if error.is_auth_error()) {
                self.mark_auth_rejected(attempted_access);
            }
            return result;
        }

        let (relay_tx, relay_rx) = flume::unbounded();
        let attempt = operation(request_auth, oauth, relay_tx);
        let forward = async move {
            let mut forwarded = 0usize;
            while let Ok(event) = relay_rx.recv_async().await {
                forwarded += usize::from(event.is_content());
                if event_tx.send_async(event).await.is_err() {
                    break;
                }
            }
            forwarded
        };
        let (result, forwarded) = futures_lite::future::zip(attempt, forward).await;
        let Err(error) = result else {
            return result;
        };
        if !error.is_auth_error() {
            return Err(error);
        }
        self.mark_auth_rejected(attempted_access);
        if forwarded != 0 {
            return Err(error);
        }

        let retry_snapshot = self.refresh_oauth(self.rejected_auth_credentials()).await?;
        let retry_auth = self.request_auth(&retry_snapshot, coding_plan);
        let retry_access = bearer_token(&retry_auth);
        let retry = operation(
            retry_auth,
            retry_snapshot.oauth_tokens.is_some(),
            event_tx.clone(),
        )
        .await;
        if matches!(&retry, Err(error) if error.is_auth_error()) {
            self.mark_auth_rejected(retry_access);
        }
        retry
    }
}

impl OpenAi {
    /// What one attempt posts with the auth [`Self::request_auth`] resolved
    /// for it, for the send and the dry run alike. `oauth` comes from the same
    /// snapshot as `auth`.
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
        if is_codex_model(&model.id) {
            return Ok(WireRequest::post(
                super::responses::responses_url(auth)?,
                responses_body(oauth, model, messages, system, tools, opts, cache_key),
            ));
        }
        let mut body = self.compat.build_body(model, messages, system, tools);
        opts.thinking.apply_reasoning_effort(&mut body, model);
        apply_prompt_cache_key(&mut body, cache_key);
        apply_fast_mode(&mut body, model, opts);
        Ok(WireRequest::post(self.compat.chat_url(auth), body))
    }
}

/// The Codex backend a login talks to answers `prompt_cache_breakpoint`
/// with `not supported on this model` for every 5.6 model, so only the
/// metered API gets the breakpoint.
fn responses_body(
    oauth: bool,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    opts: &RequestOptions,
    cache_key: Option<&CacheKey>,
) -> Value {
    let mut body = super::responses::build_body(model, messages, system, tools);
    if supports_explicit_cache(&model.id) && !oauth {
        super::responses::apply_system_breakpoint(&mut body);
    }
    super::responses::apply_responses_reasoning(&mut body, &opts.thinking, model);
    apply_prompt_cache_key(&mut body, cache_key);
    apply_fast_mode(&mut body, model, opts);
    body
}

/// Only the Codex backend reads the affinity header; an API-key request to
/// the platform carries the body field alone.
fn with_codex_affinity(mut auth: ResolvedAuth, cache_key: Option<&CacheKey>) -> ResolvedAuth {
    if auth.base_url.as_deref() == Some(auth::CODING_PLAN_BASE_URL)
        && let Some(key) = cache_key
    {
        auth.headers
            .push((CODEX_SESSION_HEADER.into(), key.as_str().into()));
    }
    auth
}

fn bearer_token(auth: &ResolvedAuth) -> Option<String> {
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

fn usage_percentage(percentage: f64) -> Option<u32> {
    percentage
        .is_finite()
        .then(|| percentage.round().clamp(0.0, 100.0) as u32)
}

fn usage_label(seconds: u64) -> String {
    if seconds == SECONDS_PER_WEEK {
        return "Weekly usage".into();
    }
    if seconds == SECONDS_PER_DAY {
        return "Daily usage".into();
    }
    if seconds.is_multiple_of(SECONDS_PER_DAY) {
        return format!("{}-day usage", seconds / SECONDS_PER_DAY);
    }
    if seconds.is_multiple_of(SECONDS_PER_HOUR) {
        return format!("{}-hour usage", seconds / SECONDS_PER_HOUR);
    }
    format!("{seconds}-second usage")
}

fn usage_limit(window: CodexUsageWindow) -> Option<UsageLimit> {
    Some(UsageLimit {
        label: usage_label(window.limit_window_seconds?),
        percentage: usage_percentage(window.used_percent?),
        reset_at: window
            .reset_at
            .and_then(|seconds| seconds.checked_mul(MILLIS_PER_SECOND)),
        detail: None,
    })
}

impl From<CodexUsage> for ProviderUsage {
    fn from(usage: CodexUsage) -> Self {
        let limits = [
            usage.rate_limit.primary_window,
            usage.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        .filter_map(usage_limit)
        .collect();
        Self {
            plan: usage.plan_type,
            limits,
        }
    }
}

fn parse_usage(response: &str) -> Result<ProviderUsage, AgentError> {
    let usage: ProviderUsage = serde_json::from_str::<CodexUsage>(response)?.into();
    if usage.plan.is_none() && usage.limits.is_empty() {
        return Err(AgentError::Config {
            message: EMPTY_USAGE_ERROR.into(),
        });
    }
    Ok(usage)
}

fn resolve_openai_base_url() -> Option<String> {
    let config = caudra_config::providers::ProvidersConfig::load();
    caudra_config::providers::configured_base_url("openai", config.get("openai"))
}

impl Provider for OpenAi {
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
            let codex = is_codex_model(&model.id);
            let stream_timeout = self.compat.stream_timeout();
            self.with_oauth_stream_retry(codex, event_tx, |auth, oauth, attempt_tx| {
                let wire = self.request(
                    oauth, &auth, model, messages, system, tools, &opts, cache_key,
                );
                async move {
                    let wire = wire?;
                    if !codex {
                        return self
                            .compat
                            .do_stream(model, &[], &wire, &attempt_tx, &auth)
                            .await;
                    }
                    super::responses::do_stream(
                        self.compat.client(),
                        model,
                        &wire,
                        &attempt_tx,
                        &with_codex_affinity(auth, cache_key),
                        stream_timeout,
                    )
                    .await
                }
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
        let snapshot = self.auth_snapshot();
        let auth = self.request_auth(&snapshot, is_codex_model(&model.id));
        self.request(
            snapshot.oauth_tokens.is_some(),
            &auth,
            model,
            messages,
            system,
            tools,
            opts,
            cache_key,
        )
    }

    fn reasoning_transport(&self, model: &Model) -> crate::ReasoningTransport {
        if is_codex_model(&model.id) {
            crate::ReasoningTransport::OpenAiResponses
        } else {
            crate::ReasoningTransport::OpenAiChatCompletions
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            if self.is_oauth() {
                return Ok(coding_plan_models());
            }
            self.with_oauth_retry(false, |auth| async move {
                self.compat.do_list_models(&auth).await
            })
            .await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            if !self.is_oauth() {
                return Ok(None);
            }
            self.with_oauth_retry(true, |auth| async move {
                let response = self.compat.get_text(&auth, USAGE_URL).await?;
                Ok(Some(parse_usage(&response)?))
            })
            .await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                let mut rejected = self.rejected_auth_credentials();
                if rejected.is_empty()
                    && let Some(access) = bearer_token(&self.auth_snapshot().resolved)
                {
                    rejected.push(access);
                }
                self.refresh_oauth(rejected).await.map(|_| ())
            } else {
                self.reload_auth().await
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(storage) = self.storage.clone() else {
                return Ok(());
            };
            let _update = self.auth_update.lock().await;
            let (resolved, oauth_tokens) =
                smol::unblock(move || auth::resolve_with_tokens(&storage)).await?;
            self.install_resolved_auth(resolved, oauth_tokens);
            let snapshot = self.auth_snapshot();
            let mut rejected = self.rejected_auth_credentials.lock().unwrap();
            if bearer_token(&snapshot.resolved).is_none_or(|access| !rejected.contains(&access)) {
                rejected.clear();
            }
            debug!("reloaded OpenAI auth from storage");
            Ok(())
        })
    }

    fn reload_auth_if_changed(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            let previous = self.auth_snapshot();
            self.reload_auth().await?;
            let current = self.auth_snapshot();
            Ok(previous != current || self.auth_replacement_available(&current))
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        model.billing = Billing::from_oauth(self.is_oauth());
        let baseline_context_window = *self
            .model_context_windows
            .lock()
            .unwrap()
            .entry(model.id.clone())
            .or_insert(model.context_window);
        if self.is_oauth()
            && let Some(plan_context_window) = coding_plan_context_window(&model.id)
        {
            // The plan windows are `total - max_output_tokens`, so the output
            // allowance sits on top rather than inside them. That holds only
            // where the plan window is the one in force: a model whose own
            // window is narrower keeps its own terms.
            model.window_excludes_output = plan_context_window <= baseline_context_window;
            model.context_window = baseline_context_window.min(plan_context_window);
        } else {
            model.context_window = baseline_context_window;
            model.window_excludes_output = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use test_case::test_case;

    use super::super::responses::{self, RESPONSES_PATH};
    use super::*;
    use crate::ThinkingConfig;
    use crate::providers::Timeouts;
    use crate::providers::openai_compat::CHAT_COMPLETIONS_PATH;
    use crate::providers::test_support::CREDENTIAL_IN_URL;

    const TEST_ACCESS: &str = "test-access";
    const TEST_REFRESH: &str = "test-refresh";
    const TEST_AUTH_STATUS: u16 = 401;
    const TEST_AUTH_ERROR: &str = "expired";
    const CACHE_KEY: &str = "session/task";
    const SYSTEM_PROMPT: &str = "You are a careful engineer.";
    const TEST_BASE_URL: &str = "https://api.openai.test/v1";
    const CODEX_MODEL: &str = "openai/gpt-5.3-codex";
    const CHAT_MODEL: &str = "openai/gpt-4.1";
    const MISSING_PLAN_MODEL: &str =
        "a model named in PLAN_MODELS must reach the coding-plan listing";
    const UNENTITLED_PLAN_MODEL: &str =
        "the coding-plan listing offered a model the subscription cannot run";
    const DUPLICATE_PLAN_MODEL: &str =
        "a model both tabled and published by the catalog must be listed once";

    fn effort(level: &str) -> ThinkingConfig {
        ThinkingConfig::Effort(level.into())
    }

    fn oauth_tokens() -> OAuthTokens {
        OAuthTokens {
            access: TEST_ACCESS.into(),
            refresh: TEST_REFRESH.into(),
            expires: u64::MAX,
            account_id: None,
        }
    }

    fn session_header(auth: &ResolvedAuth) -> Option<&str> {
        auth.headers
            .iter()
            .find(|(name, _)| name == CODEX_SESSION_HEADER)
            .map(|(_, value)| value.as_str())
    }

    #[test_case(true, true, Some(CACHE_KEY) ; "codex_backend_routes_by_the_key")]
    #[test_case(true, false, None ; "codex_backend_without_a_key")]
    #[test_case(false, true, None ; "api_key_platform_never_gets_the_header")]
    fn codex_affinity_header_follows_the_cache_key(
        coding_plan: bool,
        keyed: bool,
        expected: Option<&str>,
    ) {
        let auth = if coding_plan {
            auth::build_coding_plan_resolved(&oauth_tokens())
        } else {
            ResolvedAuth::bearer(TEST_ACCESS)
        };
        let key = keyed.then(|| CacheKey::task(None, CACHE_KEY));

        let auth = with_codex_affinity(auth, key.as_ref());

        assert_eq!(session_header(&auth), expected);
    }

    #[test_case("gpt-6-astra")]
    #[test_case("gpt-5.6-luna")]
    #[test_case("gpt-5.6-terra")]
    #[test_case("gpt-5.6-sol")]
    fn named_plan_models_use_coding_plan(model_id: &str) {
        assert!(is_codex_model(model_id));
    }

    #[test_case("gpt-6-astra", true)]
    #[test_case("gpt-5.6-luna", true)]
    #[test_case("gpt-5.6-sol", true)]
    #[test_case("gpt-5.5", false ; "gpt_5_5_is_implicit_only")]
    #[test_case("gpt-5.3-codex", false ; "codex_before_5_6_is_implicit_only")]
    #[test_case("gpt-5.4-nano", false)]
    fn explicit_cache_breakpoints_are_sent_to_gpt_5_6_and_later(model_id: &str, expected: bool) {
        assert_eq!(supports_explicit_cache(model_id), expected);
    }

    /// Verified live: the Codex backend 400s on the field for every 5.6 model,
    /// so a login keeps `instructions` while an API key gets the marked
    /// developer message.
    #[test_case("openai/gpt-5.6-sol", false, true ; "api_key_marks_a_5_6_model")]
    #[test_case("openai/gpt-5.6-sol", true, false ; "login_keeps_instructions_on_a_5_6_model")]
    #[test_case("openai/gpt-5.5", false, false ; "api_key_keeps_instructions_before_5_6")]
    fn responses_body_marks_the_system_prompt_only_for_the_metered_api(
        spec: &str,
        oauth: bool,
        marked: bool,
    ) {
        let provider = provider_with_login(oauth);
        let model = Model::from_spec(spec).unwrap();

        let wire = provider
            .wire_request(
                &model,
                &[],
                SYSTEM_PROMPT,
                &Value::Null,
                &RequestOptions::default(),
                None,
            )
            .unwrap();

        assert_eq!(
            wire.body.get(responses::INSTRUCTIONS_FIELD).is_none(),
            marked
        );
        assert_eq!(
            wire.body["input"][0]["content"][0][responses::CACHE_BREAKPOINT_FIELD].is_object(),
            marked
        );
    }

    fn provider_with_login(oauth: bool) -> OpenAi {
        let provider = OpenAi::with_auth(
            Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(TEST_BASE_URL.into()),
                ..ResolvedAuth::bearer(TEST_ACCESS)
            })),
            Timeouts::default(),
        );
        if oauth {
            provider.auth_state.lock().unwrap().oauth_tokens = Some(oauth_tokens());
        }
        provider
    }

    /// A login runs Codex models on the Codex backend, a key runs them on the
    /// platform, and everything else speaks Chat Completions.
    #[test_case(CODEX_MODEL, false, TEST_BASE_URL, RESPONSES_PATH ; "a_key_posts_codex_models_to_the_platform")]
    #[test_case(CODEX_MODEL, true, auth::CODING_PLAN_BASE_URL, RESPONSES_PATH ; "a_login_posts_codex_models_to_the_codex_backend")]
    #[test_case(CHAT_MODEL, false, TEST_BASE_URL, CHAT_COMPLETIONS_PATH ; "other_models_post_chat_completions")]
    fn wire_request_posts_where_the_send_does(spec: &str, oauth: bool, base: &str, path: &str) {
        let wire = provider_with_login(oauth)
            .wire_request(
                &Model::from_spec(spec).unwrap(),
                &[],
                SYSTEM_PROMPT,
                &Value::Null,
                &RequestOptions::default(),
                None,
            )
            .unwrap();

        assert_eq!(wire.url, format!("{base}{path}"));
        assert!(!wire.url.contains(TEST_ACCESS), "{CREDENTIAL_IN_URL}");
    }

    #[test_case("gpt-6-astra", Some(372_000))]
    #[test_case("gpt-6-sol", Some(372_000))]
    #[test_case("gpt-6-luna", Some(372_000))]
    #[test_case("gpt-5.6-luna", Some(372_000))]
    #[test_case("gpt-5.6-terra", Some(372_000))]
    #[test_case("gpt-5.6-sol", Some(372_000))]
    #[test_case("gpt-5.5", Some(272_000))]
    #[test_case("gpt-5.3-codex", Some(272_000))]
    #[test_case("gpt-5.7-codex", Some(272_000) ; "unlisted codex model still routes")]
    #[test_case("gpt-5.6-terra-preview", None ; "non-codex near-match is rejected")]
    #[test_case("gpt-6-astra-preview", None ; "non-codex gpt-6 near-match is rejected")]
    #[test_case("gpt-5.4-nano", None)]
    fn coding_plan_context_window_resolves_plan_models(model_id: &str, expected: Option<u32>) {
        assert_eq!(coding_plan_context_window(model_id), expected);
    }

    /// The OAuth listing draws on models.dev to pick up releases the static
    /// table has not reached, but the catalog describes the metered API. An id
    /// it publishes that the subscription cannot run must not be offered.
    #[test]
    fn the_coding_plan_listing_offers_only_entitled_models() {
        let ids: Vec<String> = coding_plan_models().into_iter().map(|m| m.id).collect();

        assert!(
            ids.iter().any(|id| id == "gpt-6-astra"),
            "{MISSING_PLAN_MODEL}"
        );
        assert!(
            ids.iter().all(|id| is_codex_model(id)),
            "{UNENTITLED_PLAN_MODEL}"
        );
        assert!(
            ids.windows(2).all(|pair| pair[0] != pair[1]),
            "{DUPLICATE_PLAN_MODEL}"
        );
    }

    #[test_case("openai/gpt-6-sol", true, Some(PRIORITY_SERVICE_TIER) ; "fast_asks_for_the_priority_tier")]
    #[test_case("openai/gpt-6-luna", true, Some(PRIORITY_SERVICE_TIER) ; "luna_sells_a_fast_tier_too")]
    #[test_case("openai/gpt-6-astra", true, Some(PRIORITY_SERVICE_TIER) ; "astra_sells_a_fast_tier")]
    #[test_case("openai/gpt-5.6-sol", true, Some(PRIORITY_SERVICE_TIER) ; "gpt_5_6_sells_a_fast_tier")]
    #[test_case("openai/gpt-6-sol", false, None ; "standard_sends_no_tier")]
    #[test_case("openai/gpt-5.5", true, None ; "a_model_without_fast_pricing_stays_standard")]
    fn fast_mode_sets_the_service_tier(spec: &str, fast: bool, expected: Option<&str>) {
        let model = Model::from_spec(spec).unwrap();
        let mut body = json!({});

        apply_fast_mode(
            &mut body,
            &model,
            &RequestOptions {
                fast,
                ..Default::default()
            },
        );

        assert_eq!(body.get("service_tier").and_then(Value::as_str), expected);
    }

    #[test]
    fn coding_plan_context_window_is_restored_after_oauth() {
        let provider = OpenAi::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer(TEST_ACCESS))),
            Timeouts::default(),
        );
        let mut model = Model::from_spec("openai/gpt-5.6-sol").unwrap();
        let baseline_context_window = model.context_window;
        provider.adjust_model(&mut model);

        provider.auth_state.lock().unwrap().oauth_tokens = Some(OAuthTokens {
            access: TEST_ACCESS.into(),
            refresh: TEST_REFRESH.into(),
            expires: u64::MAX,
            account_id: None,
        });
        provider.adjust_model(&mut model);
        assert_eq!(
            model.context_window,
            baseline_context_window.min(coding_plan_context_window(&model.id).unwrap())
        );
        assert!(model.window_excludes_output);

        provider.auth_state.lock().unwrap().oauth_tokens = None;
        provider.adjust_model(&mut model);
        assert_eq!(model.context_window, baseline_context_window);
        assert!(!model.window_excludes_output);
    }

    #[test]
    fn replacement_auth_remains_visible_to_all_waiters() {
        let provider = OpenAi::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer(TEST_ACCESS))),
            Timeouts::default(),
        );
        provider.mark_auth_rejected(Some(TEST_ACCESS.into()));
        assert!(!provider.auth_replacement_available(&provider.auth_snapshot()));

        provider.install_resolved_auth(ResolvedAuth::bearer("replacement"), None);
        assert!(provider.auth_replacement_available(&provider.auth_snapshot()));
    }

    #[test_case("gpt-5.3-codex", ThinkingConfig::Adaptive, None ; "adaptive_lets_the_api_decide")]
    #[test_case("gpt-5.3-codex", effort("minimal"), Some("none") ; "minimal_snaps_down_to_the_declared_floor")]
    #[test_case("gpt-5.3-codex", effort("low"), Some("low") ; "low")]
    #[test_case("gpt-5.3-codex", effort("medium"), Some("medium") ; "medium")]
    #[test_case("gpt-5.3-codex", effort("high"), Some("high") ; "high")]
    #[test_case("gpt-5.3-codex", effort("xhigh"), Some("xhigh") ; "xhigh")]
    #[test_case("gpt-5.3-codex", effort("max"), Some("xhigh") ; "max_snaps_to_the_declared_top")]
    #[test_case("gpt-5.1-codex", effort("xhigh"), Some("high") ; "xhigh_snaps_to_high_on_5_1")]
    #[test_case("gpt-5.1-codex-max", effort("xhigh"), Some("xhigh") ; "xhigh_passes_through_on_5_1_max")]
    #[test_case("gpt-5.5", effort("xhigh"), Some("xhigh") ; "xhigh_passes_through_on_5_5")]
    #[test_case("gpt-5.5", ThinkingConfig::Off, Some("none") ; "off_is_explicit_on_5_5")]
    #[test_case("gpt-5.5", effort("max"), Some("xhigh") ; "max_snaps_to_xhigh_on_5_5")]
    #[test_case("gpt-5.6-sol", ThinkingConfig::Off, Some("none") ; "off_is_explicit_on_5_6_sol")]
    #[test_case("gpt-5.6-sol", effort("max"), Some("max") ; "max_passes_through_on_5_6_sol")]
    #[test_case("gpt-5.6-terra", effort("max"), Some("max") ; "max_passes_through_on_5_6_terra")]
    #[test_case("gpt-5.6-luna", effort("max"), Some("max") ; "max_passes_through_on_5_6_luna")]
    fn responses_reasoning_uses_the_levels_the_model_declares(
        model_id: &str,
        thinking: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let model = Model::from_spec(&format!("openai/{model_id}")).unwrap();
        let mut body = json!({});
        responses::apply_responses_reasoning(&mut body, &thinking, &model);
        match expected {
            Some(level) => assert_eq!(body["reasoning"]["effort"], level),
            None => assert!(body["reasoning"].get("effort").is_none()),
        }
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert!(body.get("reasoning_effort").is_none());
    }

    const UNDECLARED: &str =
        "plan model has no declared reasoning levels, so effort is unreachable";

    #[test]
    fn every_plan_model_declares_its_levels() {
        for model_id in PLAN_MODELS {
            let model = Model::from_spec(&format!("openai/{model_id}")).unwrap();
            assert!(
                !model.reasoning_options().efforts().is_empty(),
                "{UNDECLARED}: {model_id}",
            );
        }
    }

    #[test]
    fn stream_auth_error_after_event_is_not_retried() {
        smol::block_on(async {
            let provider = OpenAi::with_auth(
                Arc::new(Mutex::new(ResolvedAuth::bearer(TEST_ACCESS))),
                Timeouts::default(),
            );
            provider.auth_state.lock().unwrap().oauth_tokens = Some(OAuthTokens {
                access: TEST_ACCESS.into(),
                refresh: TEST_REFRESH.into(),
                expires: u64::MAX,
                account_id: None,
            });
            let calls = AtomicUsize::new(0);
            let (event_tx, event_rx) = flume::unbounded();

            let error = provider
                .with_oauth_stream_retry(false, &event_tx, |_, _, attempt_tx| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        attempt_tx
                            .send(ProviderEvent::TextDelta {
                                text: "partial".into(),
                            })
                            .unwrap();
                        Err(AgentError::api(TEST_AUTH_STATUS, TEST_AUTH_ERROR))
                    }
                })
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                AgentError::Api {
                    status: TEST_AUTH_STATUS,
                    ..
                }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(matches!(
                event_rx.recv().unwrap(),
                ProviderEvent::TextDelta { .. }
            ));
        });
    }

    #[test]
    fn codex_usage_parses_quota_windows() {
        const RESPONSE: &str = r#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.6,
                    "limit_window_seconds": 18000,
                    "reset_at": 1760000000
                },
                "secondary_window": {
                    "used_percent": 120,
                    "limit_window_seconds": 604800,
                    "reset_at": 1760100000
                }
            }
        }"#;
        let usage = parse_usage(RESPONSE).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(
            usage.limits,
            vec![
                UsageLimit {
                    label: "5-hour usage".into(),
                    percentage: Some(13),
                    reset_at: Some(1_760_000_000_000),
                    detail: None,
                },
                UsageLimit {
                    label: "Weekly usage".into(),
                    percentage: Some(100),
                    reset_at: Some(1_760_100_000_000),
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn codex_usage_skips_incomplete_windows() {
        const RESPONSE: &str = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 10},
                "secondary_window": {"used_percent": -2, "limit_window_seconds": 86400}
            }
        }"#;
        let usage = parse_usage(RESPONSE).unwrap();
        assert_eq!(
            usage.limits,
            vec![UsageLimit {
                label: "Daily usage".into(),
                percentage: Some(0),
                reset_at: None,
                detail: None,
            }]
        );
    }

    #[test_case("{}")]
    #[test_case(r#"{"rate_limit": {}}"#)]
    fn codex_usage_rejects_empty_responses(response: &str) {
        assert_eq!(
            parse_usage(response).unwrap_err().to_string(),
            EMPTY_USAGE_ERROR
        );
    }
}
