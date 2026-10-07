use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use proptest::prelude::*;

use crate::{HostClassification, IpClassification, UrlPolicy, classify_hostname, classify_ip};

const PROPERTY_CASES: u32 = 96;

fn ipv6_with_tail(first: u8, second: u8, tail: [u8; 14]) -> Ipv6Addr {
    let mut octets = [0_u8; 16];
    octets[0] = first;
    octets[1] = second;
    octets[2..].copy_from_slice(&tail);
    Ipv6Addr::from(octets)
}

fn mixed_ascii_case(value: &str, case_bits: u64) -> String {
    value
        .bytes()
        .enumerate()
        .map(|(index, byte)| {
            if byte.is_ascii_alphabetic() && (case_bits.rotate_right(index as u32) & 1) == 1 {
                byte.to_ascii_uppercase()
            } else {
                byte
            }
        })
        .map(char::from)
        .collect()
}

fn special_suffix() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("localhost"),
        Just("local"),
        Just("localdomain"),
        Just("lan"),
        Just("home.arpa"),
        Just("test"),
        Just("invalid"),
        Just("example"),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: PROPERTY_CASES,
        max_shrink_iters: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn generated_ipv4_private_ranges_are_private(
        range in 0_u8..3,
        first_tail in any::<u8>(),
        second_tail in any::<u8>(),
        third_tail in any::<u8>(),
    ) {
        let address = match range {
            0 => Ipv4Addr::new(10, first_tail, second_tail, third_tail),
            1 => Ipv4Addr::new(172, 16 + first_tail % 16, second_tail, third_tail),
            _ => Ipv4Addr::new(192, 168, second_tail, third_tail),
        };
        prop_assert_eq!(classify_ip(IpAddr::V4(address)), IpClassification::Private);
    }

    #[test]
    fn generated_ipv4_loopback_range_is_loopback(tail in any::<[u8; 3]>()) {
        let address = Ipv4Addr::new(127, tail[0], tail[1], tail[2]);
        prop_assert_eq!(classify_ip(IpAddr::V4(address)), IpClassification::Loopback);
    }

    #[test]
    fn generated_ipv4_link_local_range_is_link_local(tail in any::<[u8; 2]>()) {
        let address = Ipv4Addr::new(169, 254, tail[0], tail[1]);
        prop_assert_eq!(classify_ip(IpAddr::V4(address)), IpClassification::LinkLocal);
    }

    #[test]
    fn generated_ipv4_multicast_range_is_multicast(
        first in 224_u8..=239,
        tail in any::<[u8; 3]>(),
    ) {
        let address = Ipv4Addr::new(first, tail[0], tail[1], tail[2]);
        prop_assert_eq!(classify_ip(IpAddr::V4(address)), IpClassification::Multicast);
    }

    #[test]
    fn generated_ipv6_unique_local_range_is_private(
        first in 0xfc_u8..=0xfd,
        second in any::<u8>(),
        tail in any::<[u8; 14]>(),
    ) {
        let address = ipv6_with_tail(first, second, tail);
        prop_assert_eq!(classify_ip(IpAddr::V6(address)), IpClassification::Private);
    }

    #[test]
    fn generated_ipv6_link_local_range_is_link_local(
        second in 0x80_u8..=0xbf,
        tail in any::<[u8; 14]>(),
    ) {
        let address = ipv6_with_tail(0xfe, second, tail);
        prop_assert_eq!(classify_ip(IpAddr::V6(address)), IpClassification::LinkLocal);
    }

    #[test]
    fn generated_ipv6_multicast_range_is_multicast(
        second in any::<u8>(),
        tail in any::<[u8; 14]>(),
    ) {
        let address = ipv6_with_tail(0xff, second, tail);
        prop_assert_eq!(classify_ip(IpAddr::V6(address)), IpClassification::Multicast);
    }

    #[test]
    fn ipv4_mapped_ipv6_classification_matches_ipv4(octets in any::<[u8; 4]>()) {
        let ipv4 = Ipv4Addr::from(octets);
        let mapped = ipv4.to_ipv6_mapped();
        prop_assert_eq!(
            classify_ip(IpAddr::V6(mapped)),
            classify_ip(IpAddr::V4(ipv4)),
        );
    }

    #[test]
    fn mapped_ipv4_loopback_remains_ipv6_loopback(tail in any::<[u8; 3]>()) {
        let ipv4 = Ipv4Addr::new(127, tail[0], tail[1], tail[2]);
        prop_assert_eq!(
            classify_ip(IpAddr::V6(ipv4.to_ipv6_mapped())),
            IpClassification::Loopback,
        );
    }

    #[test]
    fn public_policy_acceptance_matches_ipv4_classification(octets in any::<[u8; 4]>()) {
        let address = IpAddr::V4(Ipv4Addr::from(octets));
        prop_assert_eq!(
            UrlPolicy::PublicInternet.validate_ip(address).is_ok(),
            classify_ip(address).is_public(),
        );
    }

    #[test]
    fn public_policy_acceptance_matches_ipv6_classification(octets in any::<[u8; 16]>()) {
        let address = IpAddr::V6(Ipv6Addr::from(octets));
        prop_assert_eq!(
            UrlPolicy::PublicInternet.validate_ip(address).is_ok(),
            classify_ip(address).is_public(),
        );
    }

    #[test]
    fn special_use_hostname_classification_ignores_case_and_safe_normalization(
        prefix in proptest::option::of("[a-z0-9]{1,12}"),
        suffix in special_suffix(),
        case_bits in any::<u64>(),
    ) {
        let hostname = prefix.map_or_else(
            || suffix.to_owned(),
            |prefix| format!("{prefix}.{suffix}"),
        );
        let mixed = mixed_ascii_case(&hostname, case_bits);
        for variant in [
            mixed.clone(),
            format!("{mixed}."),
            format!("  {mixed}\t"),
            format!("[{mixed}]"),
        ] {
            prop_assert_eq!(
                classify_hostname(&variant),
                HostClassification::SpecialUseName,
                "variant={:?}",
                variant,
            );
        }
    }

    #[test]
    fn ordinary_hostname_case_does_not_change_classification(
        label in "[a-z][a-z0-9]{0,11}",
        suffix in prop_oneof![Just("com"), Just("org"), Just("net")],
        case_bits in any::<u64>(),
    ) {
        let hostname = format!("{label}.{suffix}");
        let mixed = mixed_ascii_case(&hostname, case_bits);
        prop_assert_eq!(classify_hostname(&mixed), classify_hostname(&hostname));
        prop_assert_eq!(classify_hostname(&hostname), HostClassification::PublicName);
    }
}
