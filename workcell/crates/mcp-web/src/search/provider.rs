use std::fmt;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::common::{ProviderError, ProviderResults};
use crate::types::WebsearchInput;
use crate::{WebToolDependencies, WebsearchBackend};

pub(crate) struct ProviderCatalogContract {
    pub description: String,
    pub properties: Map<String, Value>,
}

#[async_trait]
pub(crate) trait WebsearchProvider: Send + Sync + fmt::Debug {
    fn backend(&self) -> WebsearchBackend;

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract;

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String>;

    async fn search(
        &self,
        input: &WebsearchInput,
        query: &str,
        dependencies: &WebToolDependencies,
        cancellation: CancellationToken,
    ) -> Result<ProviderResults, ProviderError>;
}
