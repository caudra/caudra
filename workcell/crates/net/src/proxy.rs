use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;
use url::{Host, Url};

/// A rejected proxy configuration value.
///
/// No variant carries the offending value. A proxy URL is operator topology and
/// can embed credentials, and this error is rendered at startup.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ProxyConfigurationError {
    /// The value is not a URL, or is empty.
    #[error("proxy URL could not be parsed")]
    InvalidUrl,
    /// The value names a scheme other than `http` or `https`.
    #[error("proxy URL scheme must be http or https")]
    UnsupportedScheme,
    /// The value has no host, or no port could be determined.
    #[error("proxy URL has no host")]
    MissingHost,
    /// A bypass entry contains `/` but is not a valid address block.
    #[error("proxy bypass rule is not a valid address block")]
    InvalidBypassRule,
}

/// A validated proxy that requests may be dialled through.
///
/// The prepared connector is shared rather than copied, because a route
/// decision clones this value on every hop.
#[derive(Clone)]
pub struct ProxyEndpoint {
    inner: Arc<ProxyEndpointInner>,
}

struct ProxyEndpointInner {
    /// `scheme://host:port`, never carrying userinfo. Used for equality only.
    target: String,
    authenticated: bool,
    proxy: reqwest::Proxy,
}

impl ProxyEndpoint {
    /// Parse an operator-supplied proxy URL.
    ///
    /// A value without a scheme is read as `http`, because conventional clients
    /// accept `HTTP_PROXY=proxy.internal:8080`.
    pub fn parse(value: &str) -> Result<Self, ProxyConfigurationError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ProxyConfigurationError::InvalidUrl);
        }
        // `Url::parse` reads `proxy.internal:8080` as the scheme `proxy`, so a
        // scheme-less value has to be normalized before parsing, not after.
        let normalized = if trimmed.contains("://") {
            trimmed.to_owned()
        } else {
            format!("http://{trimmed}")
        };
        let url = Url::parse(&normalized).map_err(|_| ProxyConfigurationError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ProxyConfigurationError::UnsupportedScheme);
        }
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or(ProxyConfigurationError::MissingHost)?
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or(ProxyConfigurationError::MissingHost)?;
        let authenticated = !url.username().is_empty() || url.password().is_some();
        let target = format!("{}://{host}:{port}", url.scheme());
        // reqwest lifts userinfo out of the URL into a Proxy-Authorization
        // header itself, so the credential never has to be split out, decoded,
        // and carried alongside the endpoint.
        let proxy = reqwest::Proxy::all(url).map_err(|_| ProxyConfigurationError::InvalidUrl)?;
        Ok(Self {
            inner: Arc::new(ProxyEndpointInner {
                target,
                authenticated,
                proxy,
            }),
        })
    }

    /// Whether the endpoint carries proxy credentials.
    #[must_use]
    pub fn authenticated(&self) -> bool {
        self.inner.authenticated
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> &str {
        &self.inner.target
    }

    /// Apply this endpoint to a client builder.
    ///
    /// `no_proxy` runs first because it clears both the accumulated proxy list
    /// and reqwest's ambient environment detection. Adding the endpoint after
    /// that is what guarantees the only proxy any Workcell client can reach is
    /// the one resolved from operator configuration at startup.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder.no_proxy().proxy(self.inner.proxy.clone())
    }
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProxyEndpoint([CONFIGURED])")
    }
}

impl PartialEq for ProxyEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.inner.target == other.inner.target
            && self.inner.authenticated == other.inner.authenticated
    }
}

impl Eq for ProxyEndpoint {}

/// How one already-validated URL must be dialled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyRoute {
    /// Resolve and connect directly, pinning every policy-approved answer.
    Direct,
    /// Hand the target to this proxy, which owns resolution and address policy.
    Proxy(ProxyEndpoint),
}

/// Immutable outbound proxy selection for a process.
///
/// Workcell resolves hostnames itself and pins the answers into the connector
/// on a direct dial. That is impossible through a proxy, which owns resolution,
/// and pointless under an enforcing sandbox, where the guest cannot resolve at
/// all. A proxied hop therefore delegates address-level policy to the proxy and
/// keeps every check that does not require DNS.
#[derive(Clone, Default)]
pub struct ProxyConfiguration {
    http: Option<ProxyEndpoint>,
    https: Option<ProxyEndpoint>,
    bypass: Option<Arc<BypassRules>>,
}

impl ProxyConfiguration {
    /// Dial every target directly.
    #[must_use]
    pub fn direct() -> Self {
        Self::default()
    }

    /// Build from operator-supplied values.
    ///
    /// `all` supplies any scheme left unset. Empty and whitespace-only values
    /// are treated as unset, matching conventional client behavior.
    pub fn from_values(
        http: Option<&str>,
        https: Option<&str>,
        all: Option<&str>,
        bypass: Option<&str>,
    ) -> Result<Self, ProxyConfigurationError> {
        let all = parse_optional(all)?;
        let http = parse_optional(http)?.or_else(|| all.clone());
        let https = parse_optional(https)?.or(all);
        let bypass = match bypass.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => BypassRules::parse(value)?.map(Arc::new),
            None => None,
        };
        Ok(Self {
            http,
            https,
            bypass,
        })
    }

    /// Whether no proxy is configured for any scheme.
    #[must_use]
    pub const fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none()
    }

    /// Decide how one URL must be dialled. Callers validate the URL first.
    #[must_use]
    pub fn route(&self, url: &Url) -> ProxyRoute {
        let endpoint = match url.scheme() {
            "http" => self.http.as_ref(),
            "https" => self.https.as_ref(),
            _ => None,
        };
        let Some(endpoint) = endpoint else {
            return ProxyRoute::Direct;
        };
        let Some(host) = url.host() else {
            return ProxyRoute::Direct;
        };
        if self
            .bypass
            .as_ref()
            .is_some_and(|rules| rules.matches(&host))
        {
            return ProxyRoute::Direct;
        }
        ProxyRoute::Proxy(endpoint.clone())
    }
}

impl fmt::Debug for ProxyConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyConfiguration")
            .field("http", &self.http.as_ref().map(|_| "[CONFIGURED]"))
            .field("https", &self.https.as_ref().map(|_| "[CONFIGURED]"))
            .field("bypass", &self.bypass.is_some())
            .finish()
    }
}

fn parse_optional(value: Option<&str>) -> Result<Option<ProxyEndpoint>, ProxyConfigurationError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => ProxyEndpoint::parse(value).map(Some),
        None => Ok(None),
    }
}

/// Hosts that must be dialled directly despite a configured proxy.
///
/// The rules follow the conventional `NO_PROXY` format. They are matched here
/// rather than handed to the transport because the decision to resolve a name
/// locally happens before any client is built.
#[derive(Debug, Default)]
struct BypassRules {
    wildcard: bool,
    domains: Vec<String>,
    addresses: Vec<IpAddr>,
    blocks: Vec<AddressBlock>,
}

impl BypassRules {
    fn parse(value: &str) -> Result<Option<Self>, ProxyConfigurationError> {
        let mut rules = Self::default();
        for entry in value.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                rules.wildcard = true;
                continue;
            }
            // Only an address block can contain a slash, so a failure here is a
            // real typo rather than an ordinary hostname. Everything else falls
            // through to a domain rule, where an unusable entry merely sends
            // traffic to the proxy, which is the enforcing side.
            if entry.contains('/') {
                rules.blocks.push(
                    AddressBlock::parse(entry).ok_or(ProxyConfigurationError::InvalidBypassRule)?,
                );
                continue;
            }
            if let Ok(address) = unbracket(entry).parse::<IpAddr>() {
                rules.addresses.push(address);
                continue;
            }
            let domain = entry
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if !domain.is_empty() {
                rules.domains.push(domain);
            }
        }
        if !rules.wildcard
            && rules.domains.is_empty()
            && rules.addresses.is_empty()
            && rules.blocks.is_empty()
        {
            return Ok(None);
        }
        Ok(Some(rules))
    }

    fn matches(&self, host: &Host<&str>) -> bool {
        if self.wildcard {
            return true;
        }
        match host {
            Host::Domain(name) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                self.domains
                    .iter()
                    .any(|rule| name == *rule || name.ends_with(&format!(".{rule}")))
            }
            Host::Ipv4(address) => self.matches_address(IpAddr::V4(*address)),
            Host::Ipv6(address) => self.matches_address(IpAddr::V6(*address)),
        }
    }

    fn matches_address(&self, address: IpAddr) -> bool {
        self.addresses.contains(&address) || self.blocks.iter().any(|block| block.contains(address))
    }
}

/// One `address/prefix` bypass entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AddressBlock {
    network: IpAddr,
    prefix: u8,
}

impl AddressBlock {
    fn parse(value: &str) -> Option<Self> {
        let (network, prefix) = value.split_once('/')?;
        let network = unbracket(network.trim()).parse::<IpAddr>().ok()?;
        let prefix = prefix.trim().parse::<u8>().ok()?;
        let width = if network.is_ipv4() { 32 } else { 128 };
        (prefix <= width).then_some(Self { network, prefix })
    }

    fn contains(self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                masked_u128(u32::from(network).into(), self.prefix, 32)
                    == masked_u128(u32::from(address).into(), self.prefix, 32)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                masked_u128(u128::from(network), self.prefix, 128)
                    == masked_u128(u128::from(address), self.prefix, 128)
            }
            _ => false,
        }
    }
}

fn masked_u128(value: u128, prefix: u8, width: u8) -> u128 {
    if prefix == 0 {
        return 0;
    }
    value & (u128::MAX << (width - prefix))
}

fn unbracket(value: &str) -> &str {
    value.trim_start_matches('[').trim_end_matches(']')
}
