use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Parses `ADDRESS/PREFIX` into its network address, with the host bits
/// cleared, and its prefix length.
pub(crate) fn parse_cidr(value: &str) -> Option<(IpAddr, u8)> {
    let (address, prefix) = value.split_once('/')?;
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let prefix = prefix.parse::<u8>().ok()?;
    let bits = u32::from(prefix);
    let network = match address.parse::<IpAddr>().ok()? {
        IpAddr::V4(ip) if bits <= u32::BITS => IpAddr::V4(Ipv4Addr::from(
            u32::from(ip) & u32::MAX.checked_shl(u32::BITS - bits).unwrap_or(0),
        )),
        IpAddr::V6(ip) if bits <= u128::BITS => IpAddr::V6(Ipv6Addr::from(
            u128::from(ip) & u128::MAX.checked_shl(u128::BITS - bits).unwrap_or(0),
        )),
        _ => return None,
    };
    Some((network, prefix))
}

#[cfg(test)]
mod tests {
    use super::parse_cidr;
    use std::net::IpAddr;
    use test_case::test_case;

    #[test_case("10.20.30.40/16", "10.20.0.0", 16; "v4_host_bits_cleared")]
    #[test_case("0.0.0.0/0", "0.0.0.0", 0; "v4_everything")]
    #[test_case("100.64.0.2/32", "100.64.0.2", 32; "v4_single")]
    #[test_case("fd7a:115c:a1e0::1/48", "fd7a:115c:a1e0::", 48; "v6_host_bits_cleared")]
    #[test_case("::1/128", "::1", 128; "v6_single")]
    fn parse_cidr_clears_host_bits(value: &str, network: &str, prefix: u8) {
        assert_eq!(
            parse_cidr(value),
            Some((network.parse::<IpAddr>().unwrap(), prefix))
        );
    }

    #[test_case("10.0.0.0"; "no_prefix")]
    #[test_case("10.0.0.0/"; "empty_prefix")]
    #[test_case("10.0.0.0/33"; "v4_prefix_too_long")]
    #[test_case("::/129"; "v6_prefix_too_long")]
    #[test_case("10.0.0.0/+8"; "signed_prefix")]
    #[test_case("10.0.0.0/256"; "prefix_overflows")]
    #[test_case("example.com/8"; "name_not_address")]
    fn parse_cidr_rejects_malformed_networks(value: &str) {
        assert_eq!(parse_cidr(value), None);
    }
}
