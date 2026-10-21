use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, PoisonError, RwLock};

use url::{Host, Url};

use crate::{IpClassification, OperatorConfiguredPolicy, UrlPolicy, UrlPolicyError, classify_ip};

/// Tailscale's IPv6 range, `fd7a:115c:a1e0::/48`. Its IPv4 addresses are in
/// the carrier-grade NAT space.
const TAILSCALE_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0));
const TAILSCALE_V6_PREFIX: u8 = 48;

/// One exact `host:port` an operator lets model-chosen URLs reach.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateHost {
    host: Host<String>,
    port: u16,
}

/// Address space an operator lets model-chosen URLs reach, on any port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateNetwork {
    /// `127.0.0.0/8` and `::1`.
    Loopback,
    /// RFC 1918 space and IPv6 unique-local `fc00::/7`.
    Private,
    /// RFC 6598 carrier-grade NAT space, `100.64.0.0/10`.
    Cgnat,
    /// `100.64.0.0/10` and Tailscale's `fd7a:115c:a1e0::/48`.
    Tailscale,
    /// Any other network.
    Range {
        /// The network address.
        network: IpAddr,
        /// The prefix length.
        prefix: u8,
    },
}

/// Private destinations the public-internet rule would refuse but an operator
/// allows. Built from values the operator wrote, never from a URL.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrivateAllowlist {
    hosts: Vec<PrivateHost>,
    networks: Vec<PrivateNetwork>,
}

/// An allowlist that can be replaced while clients that read it keep running.
/// Each operation reads it once, so one request never sees two lists.
#[derive(Clone, Debug, Default)]
pub struct SharedAllowlist(Arc<RwLock<Arc<PrivateAllowlist>>>);

/// How a hop got past the policy, which decides how its DNS answers are
/// checked and whether the next hop may still use the allowlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    /// The base policy alone: the allowlist is closed for this hop.
    Base,
    /// The base policy, with the listed networks for addresses it refuses.
    /// `needed` says the URL itself already needed them.
    Networks { needed: bool },
    /// A host entry: the operator policy for the whole hop.
    Host,
}

impl PrivateHost {
    #[must_use]
    pub fn new(host: Host<String>, port: u16) -> Self {
        let host = match host {
            Host::Domain(name) => Host::Domain(normalized_name(&name)),
            Host::Ipv6(address) => match address.to_ipv4_mapped() {
                Some(mapped) => Host::Ipv4(mapped),
                None => Host::Ipv6(address),
            },
            ipv4 @ Host::Ipv4(_) => ipv4,
        };
        Self { host, port }
    }

    fn matches(&self, url: &Url) -> bool {
        let Some(port) = url.port_or_known_default() else {
            return false;
        };
        port == self.port
            && match url.host() {
                Some(Host::Domain(name)) => {
                    matches!(&self.host, Host::Domain(entry) if *entry == normalized_name(name))
                }
                Some(Host::Ipv4(address)) => same_address(&self.host, IpAddr::V4(address)),
                Some(Host::Ipv6(address)) => same_address(&self.host, IpAddr::V6(address)),
                None => false,
            }
    }
}

impl PrivateNetwork {
    fn contains(self, address: IpAddr) -> bool {
        match self {
            Self::Loopback => classify_ip(address) == IpClassification::Loopback,
            Self::Private => classify_ip(address) == IpClassification::Private,
            Self::Cgnat => classify_ip(address) == IpClassification::Shared,
            Self::Tailscale => {
                classify_ip(address) == IpClassification::Shared
                    || in_network(address, TAILSCALE_V6, TAILSCALE_V6_PREFIX)
            }
            Self::Range { network, prefix } => in_network(address, network, prefix),
        }
    }
}

impl PrivateAllowlist {
    #[must_use]
    pub fn new(hosts: Vec<PrivateHost>, networks: Vec<PrivateNetwork>) -> Self {
        Self { hosts, networks }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.networks.is_empty()
    }

    /// Validate `url` before DNS, under `base` widened by this list. A refused
    /// URL fails with the error `base` gives it, so an unlisted target reads
    /// exactly as it does without a list.
    pub fn validate_url(&self, base: UrlPolicy, url: &Url) -> Result<(), UrlPolicyError> {
        self.admit_url(base, url, true).map(|_| ())
    }

    /// Whether this list, rather than `base`, lets `url` through by its name or
    /// address: a host entry names it, or it is an IP literal in a listed
    /// network, or a special-use name while a network is listed.
    #[must_use]
    pub fn admits_private(&self, base: UrlPolicy, url: &Url) -> bool {
        matches!(
            self.admit_url(base, url, true),
            Ok(Admission::Host | Admission::Networks { needed: true })
        )
    }

    pub(crate) fn admit_url(
        &self,
        base: UrlPolicy,
        url: &Url,
        direct: bool,
    ) -> Result<Admission, UrlPolicyError> {
        if self.hosts.iter().any(|host| host.matches(url)) {
            return operator(base).validate_url(url).map(|()| Admission::Host);
        }
        match base.validate_url(url) {
            Ok(()) => Ok(Admission::Networks { needed: false }),
            Err(UrlPolicyError::NonPublicIp { address, .. }) if self.network_admits(address) => {
                Ok(Admission::Networks { needed: true })
            }
            Err(UrlPolicyError::SpecialUseHostname(_)) if direct && !self.networks.is_empty() => {
                Ok(Admission::Networks { needed: true })
            }
            Err(error) => Err(error),
        }
    }

    /// Validate one DNS answer or IP literal for a hop admitted as
    /// `admission`. `Ok(true)` when only this list let it through.
    pub(crate) fn admit_answer(
        &self,
        base: UrlPolicy,
        admission: Admission,
        address: IpAddr,
    ) -> Result<bool, UrlPolicyError> {
        match admission {
            Admission::Host => operator(base).validate_ip(address).map(|()| true),
            Admission::Base => base.validate_ip(address).map(|()| false),
            Admission::Networks { .. } => match base.validate_ip(address) {
                Ok(()) => Ok(false),
                Err(UrlPolicyError::NonPublicIp { .. }) if self.network_admits(address) => Ok(true),
                Err(error) => Err(error),
            },
        }
    }

    fn network_admits(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        !matches!(
            classify_ip(address),
            IpClassification::Unspecified | IpClassification::Multicast
        ) && self
            .networks
            .iter()
            .any(|network| network.contains(address))
    }
}

impl Admission {
    pub(crate) const fn needed(self) -> bool {
        match self {
            Self::Base => false,
            Self::Networks { needed } => needed,
            Self::Host => true,
        }
    }
}

impl SharedAllowlist {
    /// The list in force now.
    #[must_use]
    pub fn current(&self) -> Arc<PrivateAllowlist> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Put `allowlist` in force for every operation that starts after this.
    pub fn replace(&self, allowlist: PrivateAllowlist) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(allowlist);
    }
}

/// `base` with the exceptions a host entry grants. URL credentials stay as
/// `base` has them.
fn operator(base: UrlPolicy) -> UrlPolicy {
    UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
        allow_non_public_ips: true,
        allow_special_use_names: true,
        ..base.operator_options()
    })
}

fn normalized_name(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

fn same_address(entry: &Host<String>, address: IpAddr) -> bool {
    match (entry, address.to_canonical()) {
        (Host::Ipv4(entry), IpAddr::V4(address)) => *entry == address,
        (Host::Ipv6(entry), IpAddr::V6(address)) => *entry == address,
        _ => false,
    }
}

fn in_network(address: IpAddr, network: IpAddr, prefix: u8) -> bool {
    let prefix = u32::from(prefix);
    match (network.to_canonical(), address.to_canonical()) {
        (IpAddr::V4(network), IpAddr::V4(address)) if prefix <= u32::BITS => {
            let mask = u32::MAX.checked_shl(u32::BITS - prefix).unwrap_or(0);
            u32::from(network) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(address)) if prefix <= u128::BITS => {
            let mask = u128::MAX.checked_shl(u128::BITS - prefix).unwrap_or(0);
            u128::from(network) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use test_case::test_case;
    use url::{Host, Url};

    use super::{PrivateAllowlist, PrivateHost, PrivateNetwork};
    use crate::UrlPolicy;

    const TAILNET_URL: &str = "http://100.64.0.2:8787/flag";

    fn host(host: &str, port: u16) -> PrivateHost {
        PrivateHost::new(Host::parse(host).unwrap(), port)
    }

    #[test_case("100.64.0.2", 8787, TAILNET_URL, true; "exact_address_and_port")]
    #[test_case("100.64.0.2", 8788, TAILNET_URL, false; "other_port")]
    #[test_case("100.64.0.3", 8787, TAILNET_URL, false; "other_address")]
    #[test_case("NAS.Lan.", 80, "http://nas.lan/", true; "case_trailing_dot_and_default_port")]
    #[test_case("nas.lan", 80, "https://nas.lan/", false; "https_default_port")]
    #[test_case("[fd7a::1]", 80, "http://[fd7a:0:0::1]/", true; "ipv6_compared_as_an_address")]
    #[test_case("100.64.0.2", 8787, "http://[::ffff:100.64.0.2]:8787/", true; "mapped_ipv6_literal")]
    fn host_entries_match_host_and_port(entry: &str, port: u16, url: &str, expected: bool) {
        assert_eq!(host(entry, port).matches(&Url::parse(url).unwrap()), expected);
    }

    #[test_case(PrivateNetwork::Loopback, "127.0.0.9", true; "loopback_v4")]
    #[test_case(PrivateNetwork::Loopback, "::1", true; "loopback_v6")]
    #[test_case(PrivateNetwork::Loopback, "10.0.0.1", false; "loopback_excludes_lan")]
    #[test_case(PrivateNetwork::Private, "192.168.1.5", true; "private_rfc1918")]
    #[test_case(PrivateNetwork::Private, "fd00::5", true; "private_unique_local")]
    #[test_case(PrivateNetwork::Private, "::ffff:10.1.2.3", true; "private_mapped_v6")]
    #[test_case(PrivateNetwork::Private, "100.64.0.2", false; "private_excludes_cgnat")]
    #[test_case(PrivateNetwork::Cgnat, "100.127.255.254", true; "cgnat")]
    #[test_case(PrivateNetwork::Cgnat, "fd7a:115c:a1e0::1", false; "cgnat_excludes_tailscale_v6")]
    #[test_case(PrivateNetwork::Tailscale, "100.64.0.2", true; "tailscale_v4")]
    #[test_case(PrivateNetwork::Tailscale, "fd7a:115c:a1e0:ab12::1", true; "tailscale_v6")]
    #[test_case(PrivateNetwork::Tailscale, "fd7a:115c:a1e1::1", false; "tailscale_outside_its_48")]
    #[test_case(PrivateNetwork::Loopback, "169.254.169.254", false; "no_class_reaches_metadata")]
    #[test_case(range("10.20.0.0", 16), "10.20.255.1", true; "range_inside")]
    #[test_case(range("10.20.0.0", 16), "10.21.0.1", false; "range_outside")]
    #[test_case(range("10.20.0.0", 16), "::ffff:10.20.0.1", true; "range_mapped_v6")]
    #[test_case(range("0.0.0.0", 0), "0.0.0.0", false; "never_unspecified")]
    #[test_case(range("0.0.0.0", 0), "224.0.0.1", false; "never_multicast")]
    #[test_case(range("::", 0), "::", false; "never_unspecified_v6")]
    fn networks_admit_their_addresses(network: PrivateNetwork, address: &str, expected: bool) {
        let allowlist = PrivateAllowlist::new(Vec::new(), vec![network]);
        assert_eq!(
            allowlist.network_admits(address.parse::<IpAddr>().unwrap()),
            expected
        );
    }

    fn range(network: &str, prefix: u8) -> PrivateNetwork {
        PrivateNetwork::Range {
            network: network.parse().unwrap(),
            prefix,
        }
    }

    #[test_case(vec![host("100.64.0.2", 8787)], Vec::new(), TAILNET_URL, true; "host_entry")]
    #[test_case(Vec::new(), vec![PrivateNetwork::Cgnat], TAILNET_URL, true; "literal_in_network")]
    #[test_case(Vec::new(), vec![PrivateNetwork::Loopback], "http://localhost:3000/", true; "special_name_with_networks")]
    #[test_case(Vec::new(), vec![PrivateNetwork::Loopback], "http://example.com/", false; "public_name")]
    #[test_case(Vec::new(), Vec::new(), "http://localhost:3000/", false; "empty_list")]
    fn admits_private_names_what_only_the_list_lets_through(
        hosts: Vec<PrivateHost>,
        networks: Vec<PrivateNetwork>,
        url: &str,
        expected: bool,
    ) {
        let allowlist = PrivateAllowlist::new(hosts, networks);
        assert_eq!(
            allowlist.admits_private(UrlPolicy::PublicInternet, &Url::parse(url).unwrap()),
            expected
        );
    }

    #[test]
    fn an_unlisted_target_fails_with_the_base_policy_error() {
        let url = Url::parse("http://100.64.0.3:8787/").unwrap();
        let allowlist = PrivateAllowlist::new(
            vec![host("100.64.0.2", 8787)],
            vec![PrivateNetwork::Loopback],
        );
        assert_eq!(
            allowlist.validate_url(UrlPolicy::PublicInternet, &url),
            UrlPolicy::PublicInternet.validate_url(&url)
        );
    }

    #[test]
    fn a_host_entry_keeps_refusing_credentials() {
        let url = Url::parse("http://user:secret@100.64.0.2:8787/").unwrap();
        let allowlist = PrivateAllowlist::new(vec![host("100.64.0.2", 8787)], Vec::new());
        assert!(
            allowlist
                .validate_url(UrlPolicy::PublicInternet, &url)
                .is_err()
        );
    }
}
