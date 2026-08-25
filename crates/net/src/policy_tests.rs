use url::Url;

use crate::{IpClassification, OperatorConfiguredPolicy, UrlPolicy, UrlPolicyError, classify_ip};

#[test]
fn public_policy_rejects_credentials_and_special_names() {
    let policy = UrlPolicy::PublicInternet;
    assert_eq!(
        policy.parse_url("https://user:secret@example.com", None),
        Err(UrlPolicyError::CredentialsNotAllowed)
    );
    assert_eq!(
        policy.parse_url("https://@example.com", None),
        Err(UrlPolicyError::CredentialsNotAllowed)
    );
    let base = Url::parse("https://example.com/path").unwrap();
    assert_eq!(
        policy.parse_url("//@other.example.org/path", Some(&base)),
        Err(UrlPolicyError::CredentialsNotAllowed)
    );
    assert!(matches!(
        policy.parse_url("http://metadata.local/resource", None),
        Err(UrlPolicyError::SpecialUseHostname(_))
    ));
}

#[test]
fn operator_policy_requires_explicit_exceptions() {
    let policy = UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
        allow_non_public_ips: true,
        allow_special_use_names: true,
        allow_url_credentials: true,
    });
    assert!(
        policy
            .parse_url("http://user:secret@localhost/resource", None)
            .is_ok()
    );
}

#[test]
fn classifies_relevant_ipv4_non_public_ranges() {
    let cases = [
        ("0.1.2.3", IpClassification::Unspecified),
        ("10.0.0.1", IpClassification::Private),
        ("100.64.0.1", IpClassification::Shared),
        ("127.255.255.255", IpClassification::Loopback),
        ("169.254.169.254", IpClassification::LinkLocal),
        ("172.31.255.255", IpClassification::Private),
        ("192.0.0.9", IpClassification::Reserved),
        ("192.0.2.1", IpClassification::Documentation),
        ("192.168.1.1", IpClassification::Private),
        ("198.18.0.1", IpClassification::Benchmarking),
        ("198.51.100.1", IpClassification::Documentation),
        ("203.0.113.1", IpClassification::Documentation),
        ("224.0.0.1", IpClassification::Multicast),
        ("255.255.255.255", IpClassification::Reserved),
    ];
    for (address, expected) in cases {
        assert_eq!(classify_ip(address.parse().unwrap()), expected, "{address}");
    }
    assert_eq!(
        classify_ip("93.184.216.34".parse().unwrap()),
        IpClassification::Public
    );
}

#[test]
fn classifies_relevant_ipv6_non_public_ranges_and_mapped_ipv4() {
    let cases = [
        ("::", IpClassification::Unspecified),
        ("::1", IpClassification::Loopback),
        ("::ffff:127.0.0.1", IpClassification::Loopback),
        ("64:ff9b:1::1", IpClassification::Reserved),
        ("100::1", IpClassification::Reserved),
        ("2001::1", IpClassification::Reserved),
        ("2001:100::1", IpClassification::Reserved),
        ("2001:2::1", IpClassification::Benchmarking),
        ("2001:db8::1", IpClassification::Documentation),
        ("2002:7f00:1::", IpClassification::Reserved),
        ("3fff::1", IpClassification::Documentation),
        ("5f00::1", IpClassification::Reserved),
        ("fc00::1", IpClassification::Private),
        ("fe80::1", IpClassification::LinkLocal),
        ("ff02::1", IpClassification::Multicast),
    ];
    for (address, expected) in cases {
        assert_eq!(classify_ip(address.parse().unwrap()), expected, "{address}");
    }
    assert_eq!(
        classify_ip("2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()),
        IpClassification::Public
    );
}

#[test]
fn url_parser_normalizes_noncanonical_ipv4_before_policy() {
    let result = UrlPolicy::PublicInternet.parse_url("http://127.1/private", None);
    assert!(matches!(
        result,
        Err(UrlPolicyError::NonPublicIp {
            classification: IpClassification::Loopback,
            ..
        })
    ));
}
