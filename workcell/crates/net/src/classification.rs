use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The network scope assigned to an IP address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IpClassification {
    /// An ordinary globally routable unicast address.
    Public,
    /// The all-zero address or current-network IPv4 block.
    Unspecified,
    /// Host loopback.
    Loopback,
    /// RFC 1918 private space or RFC 4193 unique-local space.
    Private,
    /// Link-local address space.
    LinkLocal,
    /// RFC 6598 carrier-grade NAT shared space.
    Shared,
    /// Documentation-only address space.
    Documentation,
    /// Benchmarking-only address space.
    Benchmarking,
    /// Multicast address space.
    Multicast,
    /// An address reserved for protocols, transition mechanisms, or future use.
    Reserved,
}

impl IpClassification {
    /// Whether this classification is permitted by public-internet policy.
    #[must_use]
    pub const fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }
}

/// A syntactic hostname classification made before DNS is consulted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HostClassification {
    /// A domain name that may be resolved, subject to validating every answer.
    PublicName,
    /// A local or special-use name such as `localhost`, `.local`, or `.test`.
    SpecialUseName,
    /// An IP literal and its network scope.
    Ip(IpClassification),
    /// An empty or otherwise unusable hostname.
    Invalid,
}

/// Classify a hostname without resolving it.
#[must_use]
pub fn classify_hostname(hostname: &str) -> HostClassification {
    let normalized = hostname
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return HostClassification::Invalid;
    }
    if let Ok(address) = normalized.parse::<IpAddr>() {
        return HostClassification::Ip(classify_ip(address));
    }

    // Special-use names are blocked before DNS, but ordinary names are still
    // resolved and every returned address is checked. The latter is the actual
    // DNS-rebinding boundary; a suffix list alone is never sufficient.
    const SPECIAL_SUFFIXES: &[&str] = &[
        "localhost",
        "local",
        "localdomain",
        "lan",
        "home.arpa",
        "test",
        "invalid",
        "example",
    ];
    if SPECIAL_SUFFIXES.iter().any(|suffix| {
        normalized == *suffix
            || normalized
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }) {
        HostClassification::SpecialUseName
    } else {
        HostClassification::PublicName
    }
}

/// Classify IPv4 and IPv6 special-purpose ranges conservatively.
///
/// Public-internet policy is allow-by-global-scope, not block-by-famous-range.
/// This closes less obvious SSRF forms such as IPv4-mapped IPv6, benchmarking,
/// documentation, transition, multicast, and future-use addresses.
#[must_use]
pub fn classify_ip(address: IpAddr) -> IpClassification {
    match address {
        IpAddr::V4(address) => classify_ipv4(address),
        IpAddr::V6(address) => classify_ipv6(address),
    }
}

fn classify_ipv4(address: Ipv4Addr) -> IpClassification {
    let value = u32::from(address);
    if in_v4(value, [0, 0, 0, 0], 8) {
        IpClassification::Unspecified
    } else if in_v4(value, [10, 0, 0, 0], 8)
        || in_v4(value, [172, 16, 0, 0], 12)
        || in_v4(value, [192, 168, 0, 0], 16)
    {
        IpClassification::Private
    } else if in_v4(value, [100, 64, 0, 0], 10) {
        IpClassification::Shared
    } else if in_v4(value, [127, 0, 0, 0], 8) {
        IpClassification::Loopback
    } else if in_v4(value, [169, 254, 0, 0], 16) {
        IpClassification::LinkLocal
    } else if in_v4(value, [192, 0, 2, 0], 24)
        || in_v4(value, [198, 51, 100, 0], 24)
        || in_v4(value, [203, 0, 113, 0], 24)
    {
        IpClassification::Documentation
    } else if in_v4(value, [198, 18, 0, 0], 15) {
        IpClassification::Benchmarking
    } else if in_v4(value, [224, 0, 0, 0], 4) {
        IpClassification::Multicast
    } else if in_v4(value, [192, 0, 0, 0], 24)
        || in_v4(value, [192, 88, 99, 0], 24)
        || in_v4(value, [240, 0, 0, 0], 4)
    {
        IpClassification::Reserved
    } else {
        IpClassification::Public
    }
}

fn classify_ipv6(address: Ipv6Addr) -> IpClassification {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return classify_ipv4(mapped);
    }
    let value = u128::from(address);
    if address.is_unspecified() {
        IpClassification::Unspecified
    } else if address.is_loopback() {
        IpClassification::Loopback
    } else if in_v6(value, v6_network(0xfc00, 0, 0, 0), 7) {
        IpClassification::Private
    } else if in_v6(value, v6_network(0xfe80, 0, 0, 0), 10) {
        IpClassification::LinkLocal
    } else if in_v6(value, v6_network(0xff00, 0, 0, 0), 8) {
        IpClassification::Multicast
    } else if in_v6(value, v6_network(0x2001, 0x0db8, 0, 0), 32)
        || in_v6(value, v6_network(0x3fff, 0, 0, 0), 20)
    {
        IpClassification::Documentation
    } else if in_v6(value, v6_network(0x2001, 0x0002, 0, 0), 48) {
        IpClassification::Benchmarking
    } else if in_v6(value, v6_network(0x0100, 0, 0, 0), 64)
        || in_v6(value, v6_network(0x0064, 0xff9b, 1, 0), 48)
        || in_v6(value, v6_network(0x2001, 0, 0, 0), 23)
        || in_v6(value, v6_network(0x2002, 0, 0, 0), 16)
        || in_v6(value, v6_network(0x5f00, 0, 0, 0), 16)
        || !in_v6(value, v6_network(0x2000, 0, 0, 0), 3)
    {
        // Includes discard-only, local NAT64, Teredo, ORCHID, 6to4,
        // segment-routing SID, deprecated site-local, and unallocated space.
        IpClassification::Reserved
    } else {
        IpClassification::Public
    }
}

const fn in_v4(value: u32, network: [u8; 4], prefix: u32) -> bool {
    let network = u32::from_be_bytes(network);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    value & mask == network & mask
}

const fn in_v6(value: u128, network: u128, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    value & mask == network & mask
}

const fn v6_network(first: u16, second: u16, third: u16, fourth: u16) -> u128 {
    ((first as u128) << 112)
        | ((second as u128) << 96)
        | ((third as u128) << 80)
        | ((fourth as u128) << 64)
}
