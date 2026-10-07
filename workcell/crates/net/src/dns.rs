use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;

use async_trait::async_trait;
use thiserror::Error;

/// A DNS lookup failure.
#[derive(Debug, Error)]
#[error("DNS lookup failed: {0}")]
pub struct DnsError(#[from] io::Error);

impl DnsError {
    /// Construct a DNS error from an injected resolver's failure message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(io::Error::other(message.into()))
    }
}

/// Injectable asynchronous DNS resolution.
///
/// The HTTP client validates *all* returned addresses. Accepting one public
/// address while silently retaining a private answer would leave address
/// selection to the connector and reopen SSRF.
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// Resolve a hostname to all available IPv4 and IPv6 addresses.
    async fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, DnsError>;
}

/// Tokio's system DNS resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioDnsResolver;

#[async_trait]
impl DnsResolver for TokioDnsResolver {
    async fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        let addresses = tokio::net::lookup_host((hostname, 0)).await?;
        Ok(addresses
            .map(|address| address.ip())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
}
