use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use workcell_net::RetryPolicy;
use workcell_source_icons::{
    ResolveSourceIconOptions, ResolvedSourceIcon, SourceIconError, SourceIconResolver,
};

#[derive(Clone, Debug)]
pub struct IconRequest {
    pub page_url: String,
    pub html: Option<String>,
    pub cancellation: CancellationToken,
}

#[async_trait]
pub trait IconProvider: Send + Sync {
    async fn resolve(
        &self,
        request: IconRequest,
    ) -> Result<Option<ResolvedSourceIcon>, SourceIconError>;
}

/// Production icon adapter with the TypeScript network-tool limits.
#[derive(Clone, Default)]
pub struct ProductionIconProvider {
    resolver: SourceIconResolver,
}

impl ProductionIconProvider {
    #[must_use]
    pub fn new(resolver: SourceIconResolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl IconProvider for ProductionIconProvider {
    async fn resolve(
        &self,
        request: IconRequest,
    ) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
        let mut options = ResolveSourceIconOptions::new(request.page_url);
        options.html = request.html;
        options.timeout = Duration::from_millis(1_500);
        options.probe_timeout = Duration::from_millis(750);
        options.total_timeout = Duration::from_millis(1_500);
        options.max_candidates = 8;
        options.max_requests = 10;
        // Icon decoration is best-effort: avoid retry amplification and rely
        // on the resolver's hard total deadline for the complete operation.
        options.retry = RetryPolicy::disabled();
        options.cancellation = request.cancellation;
        self.resolver.resolve(options).await
    }
}
