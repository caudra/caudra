use std::collections::BTreeSet;
use std::fmt;
use std::process::Command;
use std::sync::Mutex;

use isahc::config::Configurable;
use isahc::http::Uri;
use tracing::warn;
use url::{Host, Url};

const HTTP_VARS: &[&str] = &["HTTP_PROXY", "http_proxy"];
const HTTPS_VARS: &[&str] = &["HTTPS_PROXY", "https_proxy"];
const ALL_VARS: &[&str] = &["ALL_PROXY", "all_proxy"];
const BYPASS_VARS: &[&str] = &["NO_PROXY", "no_proxy"];
const COMMAND_HTTP_VAR: &str = "http_proxy";
const COMMAND_HTTPS_VAR: &str = "https_proxy";
const COMMAND_BYPASS_VAR: &str = "no_proxy";
const BYPASS_SEPARATOR: char = ',';
const HTTP_SCHEME: &str = "http";
const HTTPS_SCHEME: &str = "https";

static WARNED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

/// The proxies Caudra's own requests use, read from a lookup the caller
/// trusts instead of from libcurl's view of the process environment.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxyEnv {
    http: Option<Uri>,
    https: Option<Uri>,
    bypass: Vec<String>,
}

impl ProxyEnv {
    /// Per-scheme variables win over `ALL_PROXY`, and uppercase wins over
    /// lowercase. An empty value is unset; an unparseable one dials directly.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&'static str]| {
            names.iter().find_map(|name| {
                lookup(name)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| (*name, value))
            })
        };
        let proxy = |names: &[&'static str]| {
            first(names)
                .or_else(|| first(ALL_VARS))
                .and_then(|(name, value)| parse_proxy(name, &value))
        };
        Self {
            http: proxy(HTTP_VARS),
            https: proxy(HTTPS_VARS),
            bypass: first(BYPASS_VARS)
                .map(|(_, value)| {
                    value
                        .split(BYPASS_SEPARATOR)
                        .map(str::trim)
                        .filter(|host| !host.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn direct() -> Self {
        Self::default()
    }

    /// A numeric loopback target never leaves the host, so it is never proxied.
    pub fn route(&self, url: &Url) -> ProxyRoute {
        if numeric_loopback(url) {
            return ProxyRoute::direct();
        }
        match url.scheme() {
            HTTP_SCHEME => self.through(&self.http),
            HTTPS_SCHEME => self.https_route(),
            _ => ProxyRoute::direct(),
        }
    }

    /// For a client that only talks to public HTTPS endpoints.
    pub fn https_route(&self) -> ProxyRoute {
        self.through(&self.https)
    }

    fn through(&self, proxy: &Option<Uri>) -> ProxyRoute {
        match proxy {
            Some(proxy) => ProxyRoute {
                proxy: Some(proxy.clone()),
                bypass: self.bypass.clone(),
            },
            None => ProxyRoute::direct(),
        }
    }

    /// Hands a `curl` child exactly these settings. They travel in its
    /// environment rather than as arguments, so proxy credentials never
    /// appear in the process list.
    pub fn configure_command(&self, command: &mut Command) {
        for name in [HTTP_VARS, HTTPS_VARS, ALL_VARS, BYPASS_VARS]
            .into_iter()
            .flatten()
        {
            command.env_remove(name);
        }
        if let Some(proxy) = &self.http {
            command.env(COMMAND_HTTP_VAR, proxy.to_string());
        }
        if let Some(proxy) = &self.https {
            command.env(COMMAND_HTTPS_VAR, proxy.to_string());
        }
        if !self.bypass.is_empty() {
            command.env(
                COMMAND_BYPASS_VAR,
                self.bypass.join(&BYPASS_SEPARATOR.to_string()),
            );
        }
    }
}

impl fmt::Debug for ProxyEnv {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyEnv")
            .field("http", &self.http.as_ref().map(redacted))
            .field("https", &self.https.as_ref().map(redacted))
            .field("bypass", &self.bypass)
            .finish()
    }
}

/// The proxy for one endpoint, set explicitly on an isahc client so libcurl
/// never consults the process environment.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxyRoute {
    proxy: Option<Uri>,
    bypass: Vec<String>,
}

impl ProxyRoute {
    pub fn direct() -> Self {
        Self::default()
    }

    pub fn is_direct(&self) -> bool {
        self.proxy.is_none()
    }

    /// Sets the bypass list even when it is empty: an unset one would let
    /// libcurl read `no_proxy` from the process environment.
    pub fn apply<C: Configurable>(&self, builder: C) -> C {
        builder
            .proxy(self.proxy.clone())
            .proxy_blacklist(self.bypass.clone())
    }
}

impl fmt::Debug for ProxyRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyRoute")
            .field("proxy", &self.proxy.as_ref().map(redacted))
            .field("bypass", &self.bypass)
            .finish()
    }
}

/// Only a literal loopback address qualifies: a name such as `localhost` can
/// resolve anywhere.
pub fn numeric_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    }
}

fn parse_proxy(name: &'static str, value: &str) -> Option<Uri> {
    let parsed = value.trim().parse::<Uri>().ok();
    if parsed.is_none()
        && WARNED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name)
    {
        warn!(
            variable = name,
            "proxy variable is not a valid URL; Caudra's requests dial directly"
        );
    }
    parsed
}

fn redacted(uri: &Uri) -> String {
    let scheme = uri.scheme_str().unwrap_or(HTTP_SCHEME);
    let host = uri.host().unwrap_or_default();
    match uri.port_u16() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use isahc::config::Configurable;
    use isahc::{HttpClient, ReadResponseExt};
    use test_case::test_case;
    use url::Url;

    use super::{ProxyEnv, ProxyRoute};

    const HTTP_PROXY: &str = "http://http-proxy.test:3128/";
    const HTTPS_PROXY: &str = "http://https-proxy.test:3128/";
    const ALL_PROXY: &str = "socks5h://all-proxy.test:1080/";
    const LOWER_PROXY: &str = "http://lower-proxy.test:3128/";
    const PUBLIC_HTTPS: &str = "https://api.example.test/v1";
    const PUBLIC_HTTP: &str = "http://api.example.test/v1";
    const PROXIED_TARGET: &str = "http://example.test/path?query=1";
    const BYPASSED_HOST: &str = "localhost";
    const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    const IO_TIMEOUT: Duration = Duration::from_secs(10);
    const PROXY_USER: &str = "user";
    const PROXY_PASSWORD: &str = "secret";
    const PROXY_BASIC: &str = "proxy-authorization: basic dxnlcjpzzwnyzxq=";

    fn env(pairs: &[(&str, &str)]) -> ProxyEnv {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        ProxyEnv::from_lookup(|name| values.get(name).cloned())
    }

    fn proxy_for(pairs: &[(&str, &str)], url: &str) -> Option<String> {
        env(pairs)
            .route(&Url::parse(url).unwrap())
            .proxy
            .map(|uri| uri.to_string())
    }

    #[test_case(&[("HTTPS_PROXY", HTTPS_PROXY), ("ALL_PROXY", ALL_PROXY)], PUBLIC_HTTPS, Some(HTTPS_PROXY); "https_scheme_beats_all")]
    #[test_case(&[("HTTP_PROXY", HTTP_PROXY), ("HTTPS_PROXY", HTTPS_PROXY)], PUBLIC_HTTP, Some(HTTP_PROXY); "http_picks_http_proxy")]
    #[test_case(&[("HTTP_PROXY", HTTP_PROXY)], PUBLIC_HTTPS, None; "http_proxy_never_carries_https")]
    #[test_case(&[("ALL_PROXY", ALL_PROXY)], PUBLIC_HTTPS, Some(ALL_PROXY); "all_is_the_fallback")]
    #[test_case(&[("all_proxy", ALL_PROXY)], PUBLIC_HTTP, Some(ALL_PROXY); "lowercase_all_is_the_fallback")]
    #[test_case(&[("HTTPS_PROXY", HTTPS_PROXY), ("https_proxy", LOWER_PROXY)], PUBLIC_HTTPS, Some(HTTPS_PROXY); "uppercase_beats_lowercase")]
    #[test_case(&[("https_proxy", LOWER_PROXY)], PUBLIC_HTTPS, Some(LOWER_PROXY); "lowercase_alone")]
    #[test_case(&[("HTTPS_PROXY", " "), ("https_proxy", LOWER_PROXY)], PUBLIC_HTTPS, Some(LOWER_PROXY); "empty_value_is_unset")]
    #[test_case(&[("HTTPS_PROXY", "not a proxy"), ("ALL_PROXY", ALL_PROXY)], PUBLIC_HTTPS, None; "unparseable_value_is_direct")]
    #[test_case(&[], PUBLIC_HTTPS, None; "nothing_set")]
    fn picks_the_proxy_for_the_scheme(pairs: &[(&str, &str)], url: &str, expected: Option<&str>) {
        assert_eq!(proxy_for(pairs, url).as_deref(), expected);
    }

    #[test_case("http://127.0.0.1:8080/v1", false; "ipv4_loopback")]
    #[test_case("http://127.8.9.10/v1", false; "ipv4_loopback_range")]
    #[test_case("https://[::1]:8443/v1", false; "ipv6_loopback")]
    #[test_case("http://localhost:8080/v1", true; "localhost_name")]
    #[test_case("http://127.0.0.1.evil.test/v1", true; "loopback_lookalike")]
    #[test_case(PUBLIC_HTTPS, true; "public_host")]
    fn only_numeric_loopback_dials_directly(url: &str, proxied: bool) {
        let every = [
            ("HTTP_PROXY", HTTP_PROXY),
            ("HTTPS_PROXY", HTTPS_PROXY),
            ("ALL_PROXY", ALL_PROXY),
        ];
        assert_eq!(proxy_for(&every, url).is_some(), proxied);
    }

    #[test_case(" a.test , ,.b.test,10.0.0.0/8 ", &["a.test", ".b.test", "10.0.0.0/8"]; "split_and_trimmed")]
    #[test_case("", &[]; "empty")]
    fn bypass_list_is_split_and_trimmed(value: &str, expected: &[&str]) {
        assert_eq!(env(&[("NO_PROXY", value)]).bypass, expected);
    }

    #[test]
    fn uppercase_bypass_beats_lowercase() {
        assert_eq!(
            env(&[("NO_PROXY", "upper.test"), ("no_proxy", "lower.test")]).bypass,
            ["upper.test"]
        );
    }

    #[test]
    fn command_gets_only_the_resolved_settings() {
        let mut command = Command::new("curl");
        env(&[
            ("HTTPS_PROXY", HTTPS_PROXY),
            ("ALL_PROXY", ALL_PROXY),
            ("NO_PROXY", "a.test,b.test"),
        ])
        .configure_command(&mut command);
        let mut envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(OsStr::to_string_lossy).map(Into::into),
                )
            })
            .collect();
        envs.sort();
        let set = |name: &str, value: &str| (name.to_owned(), Some(value.to_owned()));
        let removed = |name: &str| (name.to_owned(), None);
        assert_eq!(
            envs,
            [
                removed("ALL_PROXY"),
                removed("HTTPS_PROXY"),
                removed("HTTP_PROXY"),
                removed("NO_PROXY"),
                removed("all_proxy"),
                set("http_proxy", ALL_PROXY),
                set("https_proxy", HTTPS_PROXY),
                set("no_proxy", "a.test,b.test"),
            ]
        );
    }

    #[test]
    fn debug_never_shows_proxy_credentials() {
        let proxies = env(&[(
            "HTTPS_PROXY",
            &format!("http://{PROXY_USER}:{PROXY_PASSWORD}@proxy.test:3128"),
        )]);
        let shown = format!(
            "{proxies:?} {:?}",
            proxies.route(&Url::parse(PUBLIC_HTTPS).unwrap())
        );
        assert!(!shown.contains(PROXY_PASSWORD), "{shown}");
        assert!(shown.contains("proxy.test:3128"), "{shown}");
    }

    fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        (listener, address)
    }

    fn serve_once(listener: TcpListener) -> JoinHandle<String> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            reader.get_mut().write_all(RESPONSE.as_bytes()).unwrap();
            head.to_ascii_lowercase()
        })
    }

    fn get(route: &ProxyRoute, url: &str) -> String {
        let client = route
            .apply(HttpClient::builder().timeout(IO_TIMEOUT))
            .build()
            .unwrap();
        client.get(url).unwrap().text().unwrap()
    }

    fn route(pairs: &[(&str, &str)], url: &str) -> ProxyRoute {
        env(pairs).route(&Url::parse(url).unwrap())
    }

    #[test]
    fn requests_go_through_the_proxy() {
        let (proxy, address) = listener();
        let server = serve_once(proxy);
        let route = route(
            &[("HTTP_PROXY", &format!("http://{address}"))],
            PROXIED_TARGET,
        );
        assert_eq!(get(&route, PROXIED_TARGET), "ok");
        assert!(
            server
                .join()
                .unwrap()
                .starts_with(&format!("get {PROXIED_TARGET} http/1.1\r\n").to_ascii_lowercase())
        );
    }

    #[test]
    fn proxy_credentials_reach_the_proxy() {
        let (proxy, address) = listener();
        let server = serve_once(proxy);
        let proxy_url = format!("http://{PROXY_USER}:{PROXY_PASSWORD}@{address}");
        let route = route(&[("HTTP_PROXY", &proxy_url)], PROXIED_TARGET);
        assert_eq!(get(&route, PROXIED_TARGET), "ok");
        assert!(server.join().unwrap().contains(PROXY_BASIC));
    }

    #[test]
    fn a_bypassed_host_dials_directly() {
        let (proxy, proxy_address) = listener();
        let (origin, origin_address) = listener();
        let port = origin_address.rsplit_once(':').unwrap().1;
        let target = format!("http://{BYPASSED_HOST}:{port}/direct");
        let server = serve_once(origin);
        let route = route(
            &[
                ("HTTP_PROXY", &format!("http://{proxy_address}")),
                ("NO_PROXY", BYPASSED_HOST),
            ],
            &target,
        );
        assert!(!route.is_direct());
        assert_eq!(get(&route, &target), "ok");
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("get /direct http/1.1\r\n")
        );
        proxy.set_nonblocking(true).unwrap();
        assert!(matches!(proxy.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    }
}
