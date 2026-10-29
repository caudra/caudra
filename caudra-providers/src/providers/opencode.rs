use std::sync::{Arc, Mutex};

use caudra_storage::StateDir;
use flume::Sender;
use serde_json::Value;

use crate::accounts::AccountCredential;
use crate::model::{Model, ModelInfo};
use crate::model_data::{Current, current};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::catalog::{
    CatalogData, EndpointType, catalog_provider, catalog_request, config_error,
    free_fallback_allowed, send_catalog_request,
};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{ResolvedAuth, with_prefix};

const ZEN_SLUG: &str = "opencode";

static CATALOG_CHAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: ZEN_SLUG,
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode (Catalog)",
};

pub struct Opencode {
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    /// The account's key for Zen itself. Every other catalog provider a model
    /// routes to signs with its own.
    zen_key: Option<String>,
    system_prefix: Option<String>,
    data: Arc<Current>,
}

/// Where a `sub/model` id sends: the model as its catalog provider names it,
/// the protocol that provider speaks, and the key it takes.
struct Route {
    model: Model,
    api_format: EndpointType,
    auth: ResolvedAuth,
}

fn route(
    catalog: &CatalogData,
    model: &Model,
    auth_override: Option<&Arc<Mutex<ResolvedAuth>>>,
    zen_key: Option<&str>,
) -> Result<Route, AgentError> {
    let (sub_provider, actual_id) = model.id.split_once('/').unwrap_or((ZEN_SLUG, &model.id));
    let (meta, provider_data) = catalog.lookup(sub_provider, actual_id)?;
    let auth = match auth_override {
        None if sub_provider == ZEN_SLUG => provider_data.auth_for_key(zen_key).into_resolved(),
        _ => provider_data.resolve_auth_with_override(
            auth_override,
            &StateDir::resolve().map_err(|error| config_error(error.to_string()))?,
        ),
    }
    .ok_or_else(|| {
            config_error(format!(
                "authentication required for provider '{sub_provider}', run `caudra auth login {sub_provider}`"
            ))
        })?;
    Ok(Route {
        model: Model {
            id: actual_id.to_string(),
            max_output_tokens: Some(meta.output),
            context_window: meta.context,
            ..model.clone()
        },
        api_format: provider_data.api_format,
        auth,
    })
}

impl Opencode {
    pub fn new(account: &AccountCredential, timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Self::with_catalog(account, timeouts, current())
    }

    pub(crate) fn with_catalog(
        account: &AccountCredential,
        timeouts: super::Timeouts,
        data: Arc<Current>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data,
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: None,
            zen_key: account.optional_api_key().map(str::to_owned),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: super::Timeouts,
        data: Arc<Current>,
    ) -> Self {
        Self {
            data,
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: Some(auth),
            zen_key: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        let data = catalog_provider(ZEN_SLUG);
        smol::unblock(move || {
            let state = StateDir::resolve().map_err(|error| config_error(error.to_string()))?;
            Ok(data.map_or_else(Vec::new, |provider| {
                provider.available_models(&state, free_fallback_allowed())
            }))
        })
        .await
    }

    fn lookup(&self, model: &Model) -> Result<Route, AgentError> {
        route(
            &self.data.catalog,
            model,
            self.auth.as_ref(),
            self.zen_key.as_deref(),
        )
    }

    fn request(
        &self,
        route: &Route,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
    ) -> WireRequest {
        let mut buf = String::new();
        let system = with_prefix(&self.system_prefix, system, &mut buf);
        catalog_request(
            &self.chat_compat,
            route.api_format,
            &route.auth,
            &route.model,
            messages,
            system,
            tools,
            &opts.thinking,
        )
    }
}

impl Provider for Opencode {
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
            let route = self.lookup(model)?;
            let wire = self.request(&route, messages, system, tools, &opts);
            send_catalog_request(
                &self.chat_compat,
                route.api_format,
                &route.auth,
                &route.model,
                &wire,
                event_tx,
            )
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
        let route = self.lookup(model)?;
        Ok(self.request(&route, messages, system, tools, opts))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}
