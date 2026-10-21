use std::net::IpAddr;

use serde::Deserialize;
use thiserror::Error;
use url::Host;

use crate::network::parse_cidr;
use crate::{ConfigField, ConfigValue};

pub const ALLOW_PRIVATE_HOSTS_FIELD: &str = "allow_private_hosts";
pub const ALLOW_PRIVATE_NETWORKS_FIELD: &str = "allow_private_networks";
const LOOPBACK_CLASS: &str = "loopback";
const PRIVATE_CLASS: &str = "private";
const CGNAT_CLASS: &str = "cgnat";
const TAILSCALE_CLASS: &str = "tailscale";
const SCHEME_SEPARATOR: &str = "://";
const NOT_HOST_CHARACTERS: [char; 4] = ['/', '@', '?', '#'];
const WILDCARD: char = '*';
const HAS_SCHEME: &str = "has a scheme; write only HOST:PORT";
const NOT_ONLY_HOST: &str = "has a path, user name, query or fragment; write only HOST:PORT";
const HAS_WILDCARD: &str = "has a wildcard; name one host";
const NEEDS_PORT: &str = "needs a port from 1 to 65535, as HOST:PORT";
const NEEDS_BRACKETS: &str = "needs brackets around an IPv6 address, such as \"[fd7a::1]:80\"";
const NOT_HOST: &str = "is not a valid host name or IP address";
const NOT_NETWORK: &str =
    "is not loopback, private, cgnat, tailscale, or a CIDR such as \"10.20.0.0/16\"";

/// The `[web]` table as written. Global-only, so a repository cannot open
/// private networks to the model.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct WebFileConfig {
    pub allow_private_hosts: Option<Vec<String>>,
    pub allow_private_networks: Option<Vec<String>>,
}

impl WebFileConfig {
    pub(crate) fn merge(&mut self, overlay: Self) {
        if overlay.allow_private_hosts.is_some() {
            self.allow_private_hosts = overlay.allow_private_hosts;
        }
        if overlay.allow_private_networks.is_some() {
            self.allow_private_networks = overlay.allow_private_networks;
        }
    }
}

/// Private destinations `webfetch` may reach in local sessions, besides the
/// public internet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebConfig {
    pub allow_private_hosts: Vec<WebHost>,
    pub allow_private_networks: Vec<WebNetwork>,
}

/// One exact `HOST:PORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebHost {
    pub host: Host<String>,
    pub port: u16,
}

/// A named address class or a CIDR, reachable on any port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebNetwork {
    Loopback,
    Private,
    Cgnat,
    Tailscale,
    Range { network: IpAddr, prefix: u8 },
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid config: web.{key}: \"{entry}\" {reason}")]
pub struct WebConfigError {
    key: &'static str,
    entry: String,
    reason: &'static str,
}

impl WebConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: ALLOW_PRIVATE_HOSTS_FIELD,
            ty: "string[]",
            default: ConfigValue::Toml("[]"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Exact `HOST:PORT` targets on loopback or private networks that `webfetch` may reach, such as `\"localhost:3000\"` or `\"[fd7a::1]:80\"`. The port is required",
        },
        ConfigField {
            name: ALLOW_PRIVATE_NETWORKS_FIELD,
            ty: "string[]",
            default: ConfigValue::Toml("[]"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Networks `webfetch` may reach on any port: `loopback`, `private`, `cgnat`, `tailscale`, or a CIDR such as `\"10.20.0.0/16\"`. Every private address a name resolves to must be inside one",
        },
    ];

    pub(crate) fn from_file(file: WebFileConfig) -> Result<Self, WebConfigError> {
        Ok(Self {
            allow_private_hosts: parse_entries(
                ALLOW_PRIVATE_HOSTS_FIELD,
                file.allow_private_hosts,
                parse_host,
            )?,
            allow_private_networks: parse_entries(
                ALLOW_PRIVATE_NETWORKS_FIELD,
                file.allow_private_networks,
                parse_network,
            )?,
        })
    }
}

fn parse_entries<T: PartialEq>(
    key: &'static str,
    entries: Option<Vec<String>>,
    parse: fn(&str) -> Result<T, &'static str>,
) -> Result<Vec<T>, WebConfigError> {
    let mut parsed = Vec::new();
    for entry in entries.unwrap_or_default() {
        let value = parse(entry.trim()).map_err(|reason| WebConfigError {
            key,
            entry: entry.clone(),
            reason,
        })?;
        if !parsed.contains(&value) {
            parsed.push(value);
        }
    }
    Ok(parsed)
}

fn parse_host(entry: &str) -> Result<WebHost, &'static str> {
    if entry.contains(SCHEME_SEPARATOR) {
        return Err(HAS_SCHEME);
    }
    if entry.contains(NOT_HOST_CHARACTERS) {
        return Err(NOT_ONLY_HOST);
    }
    if entry.contains(WILDCARD) {
        return Err(HAS_WILDCARD);
    }
    let (host, port) = entry.rsplit_once(':').ok_or(NEEDS_PORT)?;
    if host.contains(':') && !host.starts_with('[') {
        return Err(NEEDS_BRACKETS);
    }
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(NEEDS_PORT);
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(NEEDS_PORT)?;
    let host = Host::parse(host).map_err(|_| NOT_HOST)?;
    Ok(WebHost { host, port })
}

fn parse_network(entry: &str) -> Result<WebNetwork, &'static str> {
    Ok(match entry {
        LOOPBACK_CLASS => WebNetwork::Loopback,
        PRIVATE_CLASS => WebNetwork::Private,
        CGNAT_CLASS => WebNetwork::Cgnat,
        TAILSCALE_CLASS => WebNetwork::Tailscale,
        cidr => {
            let (network, prefix) = parse_cidr(cidr).ok_or(NOT_NETWORK)?;
            WebNetwork::Range { network, prefix }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOW_PRIVATE_HOSTS_FIELD, ALLOW_PRIVATE_NETWORKS_FIELD, HAS_SCHEME, HAS_WILDCARD,
        NEEDS_BRACKETS, NEEDS_PORT, NOT_HOST, NOT_NETWORK, NOT_ONLY_HOST, WebConfig,
        WebConfigError, WebFileConfig, WebHost, WebNetwork,
    };
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use test_case::test_case;
    use url::Host;

    fn hosts(entries: &[&str]) -> Result<WebConfig, WebConfigError> {
        WebConfig::from_file(WebFileConfig {
            allow_private_hosts: Some(entries.iter().map(ToString::to_string).collect()),
            allow_private_networks: None,
        })
    }

    fn networks(entries: &[&str]) -> Result<WebConfig, WebConfigError> {
        WebConfig::from_file(WebFileConfig {
            allow_private_hosts: None,
            allow_private_networks: Some(entries.iter().map(ToString::to_string).collect()),
        })
    }

    #[test]
    fn web_defaults_to_nothing_private() {
        assert_eq!(
            WebConfig::from_file(WebFileConfig::default()).unwrap(),
            WebConfig::default()
        );
    }

    #[test_case("100.64.0.2:8787", Host::Ipv4(Ipv4Addr::new(100, 64, 0, 2)), 8787; "ipv4")]
    #[test_case("LocalHost:3000", Host::Domain("localhost".into()), 3000; "name_lowercased")]
    #[test_case("[fd7a::1]:80", Host::Ipv6(Ipv6Addr::new(0xfd7a, 0, 0, 0, 0, 0, 0, 1)), 80; "ipv6")]
    #[test_case(" nas.lan:443 ", Host::Domain("nas.lan".into()), 443; "trimmed")]
    fn parses_host_entries(entry: &str, host: Host<String>, port: u16) {
        assert_eq!(
            hosts(&[entry]).unwrap().allow_private_hosts,
            vec![WebHost { host, port }]
        );
    }

    #[test_case("http://localhost:3000", HAS_SCHEME; "scheme")]
    #[test_case("localhost:3000/admin", NOT_ONLY_HOST; "path")]
    #[test_case("user@localhost:3000", NOT_ONLY_HOST; "userinfo")]
    #[test_case("*.lan:80", HAS_WILDCARD; "wildcard")]
    #[test_case("localhost", NEEDS_PORT; "missing_port")]
    #[test_case("localhost:", NEEDS_PORT; "empty_port")]
    #[test_case("localhost:0", NEEDS_PORT; "port_zero")]
    #[test_case("localhost:65536", NEEDS_PORT; "port_too_large")]
    #[test_case("localhost:+80", NEEDS_PORT; "signed_port")]
    #[test_case("fd7a::1:80", NEEDS_BRACKETS; "bare_ipv6")]
    #[test_case(":80", NOT_HOST; "empty_host")]
    #[test_case("bad host:80", NOT_HOST; "space")]
    fn rejects_bad_host_entries(entry: &str, reason: &'static str) {
        assert_eq!(
            hosts(&[entry]),
            Err(WebConfigError {
                key: ALLOW_PRIVATE_HOSTS_FIELD,
                entry: entry.to_owned(),
                reason,
            })
        );
    }

    #[test_case("loopback", WebNetwork::Loopback; "loopback")]
    #[test_case("private", WebNetwork::Private; "private")]
    #[test_case("cgnat", WebNetwork::Cgnat; "cgnat")]
    #[test_case("tailscale", WebNetwork::Tailscale; "tailscale")]
    #[test_case("10.20.1.2/16", WebNetwork::Range { network: IpAddr::V4(Ipv4Addr::new(10, 20, 0, 0)), prefix: 16 }; "cidr")]
    fn parses_network_entries(entry: &str, network: WebNetwork) {
        assert_eq!(
            networks(&[entry]).unwrap().allow_private_networks,
            vec![network]
        );
    }

    #[test_case("link-local"; "unknown_class")]
    #[test_case("Loopback"; "class_case")]
    #[test_case("10.0.0.0"; "no_prefix")]
    #[test_case("10.0.0.0/33"; "prefix_too_long")]
    fn rejects_bad_network_entries(entry: &str) {
        assert_eq!(
            networks(&[entry]),
            Err(WebConfigError {
                key: ALLOW_PRIVATE_NETWORKS_FIELD,
                entry: entry.to_owned(),
                reason: NOT_NETWORK,
            })
        );
    }

    #[test]
    fn duplicate_entries_are_kept_once() {
        let config = WebConfig::from_file(WebFileConfig {
            allow_private_hosts: Some(vec!["localhost:3000".into(), "LOCALHOST:3000".into()]),
            allow_private_networks: Some(vec!["10.1.2.3/8".into(), "10.0.0.0/8".into()]),
        })
        .unwrap();
        assert_eq!(config.allow_private_hosts.len(), 1);
        assert_eq!(config.allow_private_networks.len(), 1);
    }
}
