use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caudra_storage::StateDir;
use caudra_storage::auth::OAuthTokens;
use caudra_storage::id::SessionRef;
use flume::Sender;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
};

use super::auth;
use crate::providers::ResolvedAuth;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

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
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
];

const CODEX_PLAN_CONTEXT_WINDOW: u32 = 272_000;
const GPT_5_6_PLAN_CONTEXT_WINDOW: u32 = 372_000;
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
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
    Some(if model_id.starts_with("gpt-5.6-") {
        GPT_5_6_PLAN_CONTEXT_WINDOW
    } else {
        CODEX_PLAN_CONTEXT_WINDOW
    })
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
        debug!("refreshed OpenAI OAuth token");
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

    async fn with_oauth_stream_retry<F, Fut>(
        &self,
        coding_plan: bool,
        event_tx: &Sender<ProviderEvent>,
        operation: F,
    ) -> Result<StreamResponse, AgentError>
    where
        F: Fn(ResolvedAuth, Sender<ProviderEvent>) -> Fut,
        Fut: std::future::Future<Output = Result<StreamResponse, AgentError>>,
    {
        let snapshot = self.auth_for_request().await?;
        let oauth = snapshot.oauth_tokens.is_some();
        let request_auth = self.request_auth(&snapshot, coding_plan);
        let attempted_access = bearer_token(&request_auth);
        if !oauth {
            let result = operation(request_auth, event_tx.clone()).await;
            if matches!(&result, Err(error) if error.is_auth_error()) {
                self.mark_auth_rejected(attempted_access);
            }
            return result;
        }

        let (relay_tx, relay_rx) = flume::unbounded();
        let attempt = operation(request_auth, relay_tx);
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
        let retry = operation(retry_auth, event_tx.clone()).await;
        if matches!(&retry, Err(error) if error.is_auth_error()) {
            self.mark_auth_rejected(retry_access);
        }
        retry
    }
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
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let mut buf = String::new();
            let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);

            if is_codex_model(&model.id) {
                let mut body = super::responses::build_body(model, messages, system, tools);
                super::responses::apply_responses_reasoning(&mut body, &opts.thinking, model);
                let stream_timeout = self.compat.stream_timeout();
                return self
                    .with_oauth_stream_retry(true, event_tx, |codex_auth, attempt_tx| {
                        let body = body.clone();
                        async move {
                            super::responses::do_stream(
                                self.compat.client(),
                                model,
                                &body,
                                &attempt_tx,
                                &codex_auth,
                                stream_timeout,
                            )
                            .await
                        }
                    })
                    .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking.apply_reasoning_effort(&mut body, model);
            self.with_oauth_stream_retry(false, event_tx, |auth, attempt_tx| {
                let body = body.clone();
                async move {
                    self.compat
                        .do_stream(model, &[], &body, &attempt_tx, &auth)
                        .await
                }
            })
            .await
        })
    }

    fn reasoning_transport(&self, model: &Model) -> crate::ReasoningTransport {
        if is_codex_model(&model.id) {
            crate::ReasoningTransport::OpenAiResponses
        } else {
            crate::ReasoningTransport::OpenAiChatCompletions
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            if self.is_oauth() {
                let models = super::models()
                    .iter()
                    .flat_map(|e| e.prefixes.iter())
                    .filter(|id| is_codex_model(id))
                    .map(|&s| crate::model::ModelInfo::id_only(s.to_string()))
                    .collect();
                return Ok(models);
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
        let baseline_context_window = *self
            .model_context_windows
            .lock()
            .unwrap()
            .entry(model.id.clone())
            .or_insert(model.context_window);
        if self.is_oauth()
            && let Some(plan_context_window) = coding_plan_context_window(&model.id)
        {
            model.context_window = baseline_context_window.min(plan_context_window);
            // The plan windows are `total - max_output_tokens`, so the output
            // allowance sits on top rather than inside them.
            model.window_excludes_output = model.context_window == plan_context_window;
        } else {
            model.context_window = baseline_context_window;
            model.window_excludes_output = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use test_case::test_case;

    use super::super::responses;
    use super::*;
    use crate::ThinkingConfig;

    const TEST_ACCESS: &str = "test-access";
    const TEST_REFRESH: &str = "test-refresh";
    const TEST_AUTH_STATUS: u16 = 401;
    const TEST_AUTH_ERROR: &str = "expired";

    fn effort(level: &str) -> ThinkingConfig {
        ThinkingConfig::Effort(level.into())
    }

    #[test_case("gpt-5.6-luna")]
    #[test_case("gpt-5.6-terra")]
    #[test_case("gpt-5.6-sol")]
    fn gpt_5_6_models_use_coding_plan(model_id: &str) {
        assert!(is_codex_model(model_id));
    }

    #[test_case("gpt-5.6-luna", Some(372_000))]
    #[test_case("gpt-5.6-terra", Some(372_000))]
    #[test_case("gpt-5.6-sol", Some(372_000))]
    #[test_case("gpt-5.5", Some(272_000))]
    #[test_case("gpt-5.3-codex", Some(272_000))]
    #[test_case("gpt-5.7-codex", Some(272_000) ; "unlisted codex model still routes")]
    #[test_case("gpt-5.6-terra-preview", None ; "non-codex near-match is rejected")]
    #[test_case("gpt-5.4-nano", None)]
    fn coding_plan_context_window_resolves_plan_models(model_id: &str, expected: Option<u32>) {
        assert_eq!(coding_plan_context_window(model_id), expected);
    }

    #[test]
    fn coding_plan_context_window_is_restored_after_oauth() {
        let provider = OpenAi::with_auth(
            Arc::new(Mutex::new(ResolvedAuth::bearer(TEST_ACCESS))),
            crate::providers::Timeouts::default(),
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
            crate::providers::Timeouts::default(),
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
                crate::providers::Timeouts::default(),
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
                .with_oauth_stream_retry(false, &event_tx, |_, attempt_tx| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        attempt_tx
                            .send(ProviderEvent::TextDelta {
                                text: "partial".into(),
                            })
                            .unwrap();
                        Err(AgentError::Api {
                            status: TEST_AUTH_STATUS,
                            message: TEST_AUTH_ERROR.into(),
                        })
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
