#![forbid(unsafe_code)]

//! Shared outbound HTTP policy and bounded fetch primitives.
//!
//! The crate deliberately separates policy, DNS, and transport. SSRF defenses
//! fail when URL parsing is checked once but DNS and redirects are left to an
//! automatic client. [`HttpClient`] instead validates and resolves every hop,
//! requires every answer to satisfy the selected policy, pins those answers in
//! its default transport, and never enables automatic redirects.

mod body;
mod classification;
mod deadline;
mod dns;
mod http_client;
mod http_error;
mod policy;
mod redirect;
mod retry;
mod transport;

#[cfg(test)]
mod classification_property_tests;
#[cfg(test)]
mod http_client_tests;
#[cfg(test)]
mod policy_tests;

pub use classification::{HostClassification, IpClassification, classify_hostname, classify_ip};
pub use dns::{DnsError, DnsResolver, TokioDnsResolver};
pub use http_client::{BoundedResponse, FetchOptions, HttpClient};
pub use http_error::NetError;
pub use policy::{OperatorConfiguredPolicy, UrlPolicy, UrlPolicyError};
pub use retry::{RetryPolicy, retry_after_delay};
pub use transport::{
    BodyStream, HttpTransport, ReqwestTransport, TransportError, TransportRequest,
    TransportResponse,
};

/// The user agent used by generic Workcell network operations.
pub const NETWORK_USER_AGENT: &str = "Workcell-Net/0.1";
