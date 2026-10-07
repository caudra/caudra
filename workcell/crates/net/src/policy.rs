use std::net::IpAddr;

use thiserror::Error;
use url::{Host, Url};

use crate::{HostClassification, IpClassification, classify_hostname, classify_ip};

/// Explicit exceptions available to an operator-controlled integration.
///
/// The default is intentionally as strict as public-internet policy. Enabling
/// an exception is an operator trust decision and must not be driven by URL
/// input supplied by an untrusted user.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OperatorConfiguredPolicy {
    /// Permit IP literals and DNS answers outside globally routable space.
    pub allow_non_public_ips: bool,
    /// Permit special-use names such as `localhost`, `.local`, and `.test`.
    pub allow_special_use_names: bool,
    /// Permit HTTP URL userinfo. Disabled by default to avoid secret smuggling.
    pub allow_url_credentials: bool,
}

/// Policy applied to an HTTP(S) target and every redirect target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UrlPolicy {
    /// Only public HTTP(S) targets, with no URL credentials.
    #[default]
    PublicInternet,
    /// An operator-selected policy with explicit local-network exceptions.
    OperatorConfigured(OperatorConfiguredPolicy),
}

/// A URL or host was rejected before network I/O.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum UrlPolicyError {
    /// The value is not a valid URL, including relative resolution failures.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// Only HTTP and HTTPS are network-fetchable.
    #[error("URL scheme must be http or https")]
    UnsupportedScheme,
    /// Userinfo in URLs can smuggle credentials into logs and cross-origin hops.
    #[error("URL credentials are not allowed by policy")]
    CredentialsNotAllowed,
    /// The URL has no usable host.
    #[error("URL has no usable hostname")]
    MissingHost,
    /// A local or special-use domain is not a public-internet target.
    #[error("special-use hostname is not allowed by policy: {0}")]
    SpecialUseHostname(String),
    /// The IP scope is not allowed by the selected policy.
    #[error("non-public IP address is not allowed by policy: {address} ({classification:?})")]
    NonPublicIp {
        /// The rejected address.
        address: IpAddr,
        /// Why the address is not considered public.
        classification: IpClassification,
    },
}

impl UrlPolicy {
    /// Parse an absolute URL, or resolve a relative value against `base`, then
    /// enforce scheme, credential, and syntactic host policy.
    ///
    /// DNS answers are validated later by [`crate::HttpClient`]. Keeping both
    /// checks is essential: a harmless-looking domain can resolve to loopback,
    /// and a public first hop can redirect to a private target.
    pub fn parse_url(&self, value: &str, base: Option<&Url>) -> Result<Url, UrlPolicyError> {
        if raw_value_has_userinfo(value) && !self.operator_options().allow_url_credentials {
            return Err(UrlPolicyError::CredentialsNotAllowed);
        }
        let url = match base {
            Some(base) => base
                .join(value)
                .map_err(|_| UrlPolicyError::InvalidUrl(value.to_owned()))?,
            None => Url::parse(value).map_err(|_| UrlPolicyError::InvalidUrl(value.to_owned()))?,
        };
        self.validate_url(&url)?;
        Ok(url)
    }

    /// Validate an already parsed URL without performing DNS I/O.
    pub fn validate_url(&self, url: &Url) -> Result<(), UrlPolicyError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(UrlPolicyError::UnsupportedScheme);
        }
        if has_userinfo(url) && !self.operator_options().allow_url_credentials {
            return Err(UrlPolicyError::CredentialsNotAllowed);
        }

        match url.host().ok_or(UrlPolicyError::MissingHost)? {
            Host::Domain(host) => match classify_hostname(host) {
                HostClassification::PublicName => Ok(()),
                HostClassification::SpecialUseName
                    if self.operator_options().allow_special_use_names =>
                {
                    Ok(())
                }
                HostClassification::SpecialUseName => {
                    Err(UrlPolicyError::SpecialUseHostname(host.to_owned()))
                }
                HostClassification::Ip(classification) => {
                    let address = host.parse().map_err(|_| UrlPolicyError::MissingHost)?;
                    self.validate_classified_ip(address, classification)
                }
                HostClassification::Invalid => Err(UrlPolicyError::MissingHost),
            },
            Host::Ipv4(address) => {
                self.validate_classified_ip(IpAddr::V4(address), classify_ip(IpAddr::V4(address)))
            }
            Host::Ipv6(address) => {
                self.validate_classified_ip(IpAddr::V6(address), classify_ip(IpAddr::V6(address)))
            }
        }
    }

    /// Validate a DNS answer or IP literal under this policy.
    pub fn validate_ip(&self, address: IpAddr) -> Result<(), UrlPolicyError> {
        self.validate_classified_ip(address, classify_ip(address))
    }

    fn validate_classified_ip(
        &self,
        address: IpAddr,
        classification: IpClassification,
    ) -> Result<(), UrlPolicyError> {
        if classification.is_public() || self.operator_options().allow_non_public_ips {
            Ok(())
        } else {
            Err(UrlPolicyError::NonPublicIp {
                address,
                classification,
            })
        }
    }

    const fn operator_options(self) -> OperatorConfiguredPolicy {
        match self {
            Self::PublicInternet => OperatorConfiguredPolicy {
                allow_non_public_ips: false,
                allow_special_use_names: false,
                allow_url_credentials: false,
            },
            Self::OperatorConfigured(options) => options,
        }
    }
}

fn has_userinfo(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    // `url::Url` normalizes empty `http://@host` userinfo away. Inspecting the
    // serialized authority closes that otherwise easy-to-miss credential form.
    url.as_str()
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|authority| authority.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn raw_value_has_userinfo(value: &str) -> bool {
    let value = value.trim();
    let authority = value
        .strip_prefix("//")
        .or_else(|| value.split_once("://").map(|(_, value)| value));
    authority
        .and_then(|authority| authority.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}
