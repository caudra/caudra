#![forbid(unsafe_code)]

//! Typed web search and content fetching with an optional MCP adapter.
//!
//! The production path composes `workcell-net` public/redirect policy with
//! `workcell-source-icons`. Search transport, icon resolution, time, and PDF
//! extraction remain injectable so conformance tests never require a network.
//!
//! PDF text is extracted natively with `pdf_oxide` from a six-MiB input prefix,
//! with bounded page/text work in a panic-contained blocking worker. Its layout
//! and metadata decoding are not byte-for-byte identical to TypeScript `unpdf`:
//! complex reading order, uncommon PDF string encodings, and malformed partial
//! PDFs can produce different text or no title. The MCP shape, normalization,
//! limits, attachment mode, and failure classification remain compatible.

mod blocking;
mod catalog;
mod config;
mod dependencies;
mod environment;
mod fetch;
mod group;
mod html;
mod pdf;
mod search;
mod types;

#[cfg(feature = "mcp")]
pub use catalog::catalog;
pub use catalog::specs;
pub use config::{
    SerpApiEngine, WebsearchBackend, WebsearchConfigurationIssue, WebsearchExecutionConfiguration,
    WebsearchMode,
};
pub use dependencies::{
    Clock, IconProvider, IconRequest, ProductionIconProvider, ProductionWebHttpTransport,
    SystemClock, WebHttpError, WebHttpRequest, WebHttpRequestKind, WebHttpResponse,
    WebHttpTransport, WebToolDependencies,
};
pub use environment::{
    BRAVE_API_KEY_VARIABLE, EXA_API_KEY_VARIABLE, KAGI_API_KEY_VARIABLE, SEARXNG_API_KEY_VARIABLE,
    SEARXNG_BEARER_TOKEN_VARIABLE, SEARXNG_PASSWORD_VARIABLE, SEARXNG_URL_VARIABLE,
    SEARXNG_USER_VARIABLE, SERPAPI_API_KEY_VARIABLE, SERPAPI_ENGINE_VARIABLE,
    WEB_CONFIGURATION_VARIABLES, WEBSEARCH_BACKEND_VARIABLE, WEBSEARCH_MODE_VARIABLE,
    resolve_web_configuration_with,
};
pub use fetch::WebfetchError;
pub use group::{
    BACKEND_OUTSIDE_ALL_MODE_ERROR, PreparedWebOperation, PreparedWebfetch,
    PreparedWebfetchOperation, PreparedWebsearch, PreparedWebsearchOperation,
    STALE_WEBSEARCH_CONFIGURATION_ERROR, WebOperationError, WebOperationExecution, WebToolGroup,
    WebsearchConfigurationSnapshot, WebsearchConfigurationSource,
};
pub use pdf::{NativePdfExtractor, PdfExtraction, PdfExtractionError, PdfExtractor};
pub use types::{
    TimeRange, WebExecution, WebfetchFormat, WebfetchInput, WebfetchOutput, WebfetchPdfAttachment,
    WebfetchPdfMode, WebsearchFallback, WebsearchInput, WebsearchOutput, WebsearchResult,
};
// Re-exported so a host configuring outbound egress needs only this crate.
pub use workcell_net::{
    OperatorConfiguredPolicy, PrivateAllowlist, PrivateHost, PrivateNetwork, ProxyConfiguration,
    ProxyConfigurationError, ProxyEndpoint, ProxyRoute, SharedAllowlist, UrlPolicy,
};
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};

/// Stable crate marker retained for consumers that used the scaffold.
pub const CRATE_NAME: &str = "workcell-mcp-web";
