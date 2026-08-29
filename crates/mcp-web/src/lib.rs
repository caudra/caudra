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
};
pub use dependencies::{
    Clock, IconProvider, IconRequest, ProductionIconProvider, ProductionWebHttpTransport,
    SystemClock, WebHttpError, WebHttpRequest, WebHttpRequestKind, WebHttpResponse,
    WebHttpTransport, WebToolDependencies,
};
pub use fetch::WebfetchError;
pub use group::{PreparedWebfetch, PreparedWebsearch};
pub use group::{WebToolGroup, WebsearchConfigurationSnapshot, WebsearchConfigurationSource};
pub use pdf::{NativePdfExtractor, PdfExtraction, PdfExtractionError, PdfExtractor};
pub use types::{
    TimeRange, WebExecution, WebfetchFormat, WebfetchInput, WebfetchOutput, WebfetchPdfAttachment,
    WebfetchPdfMode, WebsearchInput, WebsearchOutput, WebsearchResult,
};
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};

/// Stable crate marker retained for consumers that used the scaffold.
pub const CRATE_NAME: &str = "workcell-mcp-web";
