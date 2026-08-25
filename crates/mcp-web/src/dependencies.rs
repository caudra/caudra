mod clock;
mod http;
mod icons;

use std::sync::Arc;

use workcell_net::{OperatorConfiguredPolicy, UrlPolicy};

use crate::pdf::{NativePdfExtractor, PdfExtractor};

pub use clock::{Clock, SystemClock};
pub use http::{
    ProductionWebHttpTransport, WebHttpError, WebHttpRequest, WebHttpRequestKind, WebHttpResponse,
    WebHttpTransport,
};
pub use icons::{IconProvider, IconRequest, ProductionIconProvider};

/// Immutable dependency bundle used by `WebToolGroup::with_dependencies`.
#[derive(Clone)]
pub struct WebToolDependencies {
    pub(crate) http: Arc<dyn WebHttpTransport>,
    pub(crate) icons: Arc<dyn IconProvider>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) pdf: Arc<dyn PdfExtractor>,
    pub(crate) webfetch_policy: UrlPolicy,
    pub(crate) source_icons_enabled: bool,
}

impl Default for WebToolDependencies {
    fn default() -> Self {
        Self::production()
    }
}

impl WebToolDependencies {
    #[must_use]
    pub fn production() -> Self {
        Self::production_with_source_icons(false)
    }

    #[must_use]
    pub fn production_with_source_icons(source_icons_enabled: bool) -> Self {
        Self {
            http: Arc::new(ProductionWebHttpTransport::new()),
            icons: Arc::new(ProductionIconProvider::default()),
            clock: Arc::new(SystemClock),
            pdf: Arc::new(NativePdfExtractor),
            webfetch_policy: UrlPolicy::PublicInternet,
            source_icons_enabled,
        }
    }

    #[must_use]
    pub fn new(
        http: Arc<dyn WebHttpTransport>,
        icons: Arc<dyn IconProvider>,
        clock: Arc<dyn Clock>,
        pdf: Arc<dyn PdfExtractor>,
    ) -> Self {
        Self {
            http,
            icons,
            clock,
            pdf,
            webfetch_policy: UrlPolicy::PublicInternet,
            // Supplying an icon provider is an explicit opt-in for alternate hosts and tests.
            source_icons_enabled: true,
        }
    }

    /// Allow reserved fixture hostnames while retaining non-public IP rejection.
    /// This is intended for injected offline transports, not production I/O.
    #[must_use]
    pub fn with_fixture_hostnames(mut self) -> Self {
        self.webfetch_policy = UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
            allow_non_public_ips: false,
            allow_special_use_names: true,
            allow_url_credentials: false,
        });
        self
    }

    #[must_use]
    pub fn with_source_icons_enabled(mut self, enabled: bool) -> Self {
        self.source_icons_enabled = enabled;
        self
    }
}
