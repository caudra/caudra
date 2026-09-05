use url::Url;

use crate::{ProxyConfiguration, ProxyConfigurationError, ProxyEndpoint, ProxyRoute};

fn configuration(proxy: &str, bypass: Option<&str>) -> ProxyConfiguration {
    ProxyConfiguration::from_values(None, None, Some(proxy), bypass).unwrap()
}

fn route(configuration: &ProxyConfiguration, url: &str) -> ProxyRoute {
    configuration.route(&Url::parse(url).unwrap())
}

fn is_proxied(configuration: &ProxyConfiguration, url: &str) -> bool {
    matches!(route(configuration, url), ProxyRoute::Proxy(_))
}

#[test]
fn accepts_scheme_less_values_and_defaults_the_scheme_to_http() {
    let endpoint = ProxyEndpoint::parse("proxy.internal:8080").unwrap();
    assert_eq!(endpoint.target(), "http://proxy.internal:8080");
    assert!(!endpoint.authenticated());
}

#[test]
fn applies_the_known_default_port_per_scheme() {
    assert_eq!(
        ProxyEndpoint::parse("http://proxy.internal")
            .unwrap()
            .target(),
        "http://proxy.internal:80"
    );
    assert_eq!(
        ProxyEndpoint::parse("https://proxy.internal")
            .unwrap()
            .target(),
        "https://proxy.internal:443"
    );
}

#[test]
fn rejects_unsupported_schemes_and_unusable_values() {
    assert_eq!(
        ProxyEndpoint::parse("socks5://proxy.internal:1080"),
        Err(ProxyConfigurationError::UnsupportedScheme)
    );
    assert_eq!(
        ProxyEndpoint::parse("file:///etc/passwd"),
        Err(ProxyConfigurationError::UnsupportedScheme)
    );
    assert_eq!(
        ProxyEndpoint::parse("http://"),
        Err(ProxyConfigurationError::InvalidUrl)
    );
    assert_eq!(
        ProxyEndpoint::parse("   "),
        Err(ProxyConfigurationError::InvalidUrl)
    );
}

#[test]
fn credentials_are_recorded_but_never_rendered() {
    let endpoint = ProxyEndpoint::parse("http://operator:hunter2@proxy.internal:8080").unwrap();
    assert!(endpoint.authenticated());
    assert_eq!(endpoint.target(), "http://proxy.internal:8080");

    let rendered = format!("{endpoint:?}");
    assert_eq!(rendered, "ProxyEndpoint([CONFIGURED])");

    let configuration = ProxyConfiguration::from_values(
        None,
        None,
        Some("http://operator:hunter2@proxy:8080"),
        None,
    )
    .unwrap();
    let rendered = format!("{configuration:?}");
    assert!(!rendered.contains("hunter2"));
    assert!(!rendered.contains("operator"));
    assert!(!rendered.contains("proxy"));
}

#[test]
fn per_scheme_values_win_over_the_catch_all() {
    let configuration = ProxyConfiguration::from_values(
        Some("http://plain.internal:8080"),
        None,
        Some("http://catch.internal:3128"),
        None,
    )
    .unwrap();
    assert!(!configuration.is_direct());
    assert_eq!(
        route(&configuration, "http://example.com/"),
        ProxyRoute::Proxy(ProxyEndpoint::parse("http://plain.internal:8080").unwrap())
    );
    // The catch-all still supplies the scheme that was left unset.
    assert_eq!(
        route(&configuration, "https://example.com/"),
        ProxyRoute::Proxy(ProxyEndpoint::parse("http://catch.internal:3128").unwrap())
    );
}

#[test]
fn a_scheme_without_a_proxy_stays_direct() {
    let configuration =
        ProxyConfiguration::from_values(None, Some("http://proxy.internal:8080"), None, None)
            .unwrap();
    assert_eq!(
        route(&configuration, "http://example.com/"),
        ProxyRoute::Direct
    );
    assert!(is_proxied(&configuration, "https://example.com/"));
}

#[test]
fn empty_and_whitespace_values_are_unset() {
    let configuration =
        ProxyConfiguration::from_values(Some(""), Some("  "), None, Some("")).unwrap();
    assert!(configuration.is_direct());
    assert_eq!(
        route(&configuration, "https://example.com/"),
        ProxyRoute::Direct
    );
}

#[test]
fn bypass_matches_domains_and_their_subdomains_only() {
    let configuration = configuration(
        "http://proxy.internal:8080",
        Some("google.com, .example.org, INTERNAL.test"),
    );
    assert!(!is_proxied(&configuration, "https://google.com/"));
    assert!(!is_proxied(&configuration, "https://www.google.com/"));
    assert!(!is_proxied(&configuration, "https://api.example.org/"));
    assert!(!is_proxied(&configuration, "https://internal.test/"));
    // Case and a fully qualified trailing dot must not defeat a rule.
    assert!(!is_proxied(&configuration, "https://GOOGLE.com./"));
    assert!(is_proxied(&configuration, "https://notgoogle.com/"));
    assert!(is_proxied(&configuration, "https://google.com.evil.net/"));
}

#[test]
fn bypass_matches_ip_literals_and_address_blocks() {
    let configuration = configuration(
        "http://proxy.internal:8080",
        Some("127.0.0.1, 192.168.1.0/24, [::1], fd00::/8"),
    );
    assert!(!is_proxied(&configuration, "http://127.0.0.1/"));
    assert!(!is_proxied(&configuration, "http://192.168.1.42/"));
    assert!(!is_proxied(&configuration, "http://[::1]/"));
    assert!(!is_proxied(&configuration, "http://[fd00::5]/"));
    assert!(is_proxied(&configuration, "http://192.168.2.42/"));
    assert!(is_proxied(&configuration, "http://[2001:db8::1]/"));
    // A domain rule must not match an address, and vice versa.
    let domains = configuration_with_rules("example.com");
    assert!(is_proxied(&domains, "http://127.0.0.1/"));
}

fn configuration_with_rules(bypass: &str) -> ProxyConfiguration {
    configuration("http://proxy.internal:8080", Some(bypass))
}

#[test]
fn wildcard_bypasses_every_host() {
    let configuration = configuration_with_rules("*");
    assert!(!is_proxied(&configuration, "https://example.com/"));
    assert!(!is_proxied(&configuration, "http://10.0.0.1/"));
}

#[test]
fn zero_prefix_block_matches_its_whole_family() {
    let configuration = configuration_with_rules("0.0.0.0/0");
    assert!(!is_proxied(&configuration, "http://93.184.216.34/"));
    assert!(is_proxied(&configuration, "http://[2001:db8::1]/"));
}

#[test]
fn a_malformed_address_block_is_rejected_but_odd_hostnames_are_not() {
    assert_eq!(
        ProxyConfiguration::from_values(None, None, Some("http://proxy:8080"), Some("10.0.0.0/99"))
            .unwrap_err(),
        ProxyConfigurationError::InvalidBypassRule
    );
    assert_eq!(
        ProxyConfiguration::from_values(
            None,
            None,
            Some("http://proxy:8080"),
            Some("10.0.0.0/nope")
        )
        .unwrap_err(),
        ProxyConfigurationError::InvalidBypassRule
    );
    // A bypass list is only ever able to send traffic to the enforcing proxy,
    // so an unrecognized hostname entry is kept rather than failing startup.
    let configuration = ProxyConfiguration::from_values(
        None,
        None,
        Some("http://proxy:8080"),
        Some("weird entry,"),
    )
    .unwrap();
    assert!(is_proxied(&configuration, "https://example.com/"));
}

#[test]
fn direct_configuration_never_proxies() {
    let configuration = ProxyConfiguration::direct();
    assert!(configuration.is_direct());
    assert_eq!(
        route(&configuration, "https://example.com/"),
        ProxyRoute::Direct
    );
}
