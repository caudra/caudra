use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::catalog::{
    CatalogData, EndpointType, catalog_request, config_error, init_shared_catalog_if_needed,
    send_catalog_request, warm_catalog_data,
};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{ResolvedAuth, with_prefix};

const ZEN_SLUG: &str = "opencode";

static CATALOG_CHAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: ZEN_SLUG,
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode (Catalog)",
};

pub struct Opencode {
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
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
) -> Result<Route, AgentError> {
    let (sub_provider, actual_id) = model.id.split_once('/').unwrap_or((ZEN_SLUG, &model.id));
    let (meta, provider_data) = catalog.lookup(sub_provider, actual_id)?;
    let auth = provider_data
        .resolve_auth_with_override(auth_override, &catalog.state_dir)
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
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: None,
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: Some(auth),
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        Ok(
            smol::unblock(move || init_shared_catalog_if_needed().lock().unwrap().all_models())
                .await,
        )
    }

    /// Routes on the shared catalog, which the first request may have to fetch.
    async fn lookup(&self, model: &Model) -> Result<Route, AgentError> {
        let model = model.clone();
        let auth_override = self.auth.clone();
        smol::unblock(move || {
            route(
                &init_shared_catalog_if_needed().lock().unwrap(),
                &model,
                auth_override.as_ref(),
            )
        })
        .await
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
            let route = self.lookup(model).await?;
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
        let route = route(&*warm_catalog_data()?, model, self.auth.as_ref())?;
        Ok(self.request(&route, messages, system, tools, opts))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}
