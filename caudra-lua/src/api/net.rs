use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use caudra_lua_macro::{lua_fn, lua_table};
use futures_lite::io::AsyncReadExt;
use isahc::config::{Configurable, RedirectPolicy, ResolveMap, VersionNegotiation};
use isahc::{AsyncBody, HttpClient, Request};
use mlua::{Lua, Result as LuaResult, Table};
use url::{Host, Url};

use crate::api::util::pair::{Pair, try_pair};

use crate::plugin_permissions::PluginPermissions;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
const FALLBACK_USER_AGENT: &str = "caudra";

struct RequestParams {
    url: Url,
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    max_bytes: usize,
    retries: u32,
}

struct ResponseData {
    body: String,
    status: u16,
    content_type: String,
}

struct ResolvedTarget {
    host: String,
    port: u16,
    addresses: Vec<IpAddr>,
}

/// Make an HTTP request and return the response body. Plain `http://`
/// URLs are automatically upgraded to `https://`. Requests to private
/// or metadata IP addresses are blocked for safety.
///
/// {opts} fields:
///   `method` (string) HTTP verb (default `"GET"`).
///   `headers` (table) Header name/value pairs.
///   `body` (string) Request body.
///   `timeout` (integer) Timeout in seconds, max 120 (default 30).
///   `max_bytes` (integer) Max response size in bytes (default 5 MB).
///   `retry` (integer) Retries on 5xx errors (default 3).
///
/// The response table has three fields: `body` (string), `status`
/// (integer), and `content_type` (string).
///
/// @param url string URL starting with `http://` or `https://`.
/// @param opts table? Request options (see above).
/// @return (table?, string?) Response table, or nil plus an error string.
/// @example
/// local res, err = caudra.net.request("https://httpbin.org/get")
/// if err then
///   print("failed: " .. err)
/// else
///   print(res.status, res.body)
/// end
#[lua_fn(guard = Net)]
async fn request(lua: Lua, url: String, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let params = try_pair!(extract_request_params(&url, opts.as_ref()));
    let resp = try_pair!(do_request(params).await);
    let tbl = lua.create_table()?;
    tbl.set("body", resp.body)?;
    tbl.set("status", resp.status)?;
    tbl.set("content_type", resp.content_type)?;
    Ok((Some(tbl), None))
}

lua_table! {
    /// HTTP client for fetching web content. All traffic goes over HTTPS
    /// (plain HTTP is upgraded). Private and metadata IP addresses are
    /// blocked to prevent SSRF. Failed requests (5xx) are retried
    /// automatically.
    ///
    /// ```lua
    /// local res, err = caudra.net.request("https://example.com")
    /// if res then print(res.body) end
    /// ```
    "caudra.net" => pub(crate) fn create_net_table(perms: &PluginPermissions), DOCS [
        request(perms),
    ]
}

fn extract_request_params(url: &str, opts: Option<&Table>) -> Result<RequestParams, String> {
    let url = validate_and_upgrade_url(url)?;

    let method = opts
        .and_then(|o| o.get::<String>("method").ok())
        .unwrap_or_else(|| "GET".to_string());

    let headers = if let Some(tbl) = opts.and_then(|o| o.get::<Table>("headers").ok()) {
        let mut h = Vec::new();
        for pair in tbl.pairs::<String, String>() {
            let (k, v) = pair.map_err(|e| format!("invalid header: {e}"))?;
            h.push((k, v));
        }
        h
    } else {
        Vec::new()
    };

    let body = opts
        .and_then(|o| o.get::<String>("body").ok())
        .map(|s| s.into_bytes())
        .unwrap_or_default();

    let timeout = Duration::from_secs(
        opts.and_then(|o| o.get::<u64>("timeout").ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );

    let max_bytes = opts
        .and_then(|o| o.get::<usize>("max_bytes").ok())
        .unwrap_or(DEFAULT_MAX_BYTES);

    let retries = opts
        .and_then(|o| o.get::<u32>("retry").ok())
        .unwrap_or(MAX_RETRIES);

    Ok(RequestParams {
        url,
        method,
        headers,
        body,
        timeout,
        max_bytes,
        retries,
    })
}

fn build_request(
    url: &str,
    user_agent: &str,
    method: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<Request<AsyncBody>, String> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("User-Agent", user_agent);

    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }

    builder
        .body(AsyncBody::from(body))
        .map_err(|e| format!("request build error: {e}"))
}

async fn do_request(params: RequestParams) -> Result<ResponseData, String> {
    let RequestParams {
        url: original_url,
        mut method,
        headers,
        mut body,
        timeout,
        max_bytes,
        retries,
    } = params;
    let mut current_url = original_url.clone();
    let mut redirects = 0;

    loop {
        let target = resolve_target(&current_url)?;
        let client = build_client(&target, timeout)?;
        let response = send_with_retries(
            &client,
            current_url.as_str(),
            &method,
            &headers,
            &body,
            retries,
        )
        .await?;
        let status = response.status().as_u16();
        if !is_redirect_status(status) {
            return read_response(response, max_bytes).await;
        }
        let Some(location) = response.headers().get("location") else {
            return read_response(response, max_bytes).await;
        };
        let location = location
            .to_str()
            .map_err(|e| format!("invalid redirect location: {e}"))?;
        if redirects == MAX_REDIRECTS {
            return Err(format!("too many redirects (limit {MAX_REDIRECTS})"));
        }
        current_url = resolve_redirect(&original_url, &current_url, location)?;
        redirects += 1;

        if (status == 301 || status == 302) && method.eq_ignore_ascii_case("POST")
            || status == 303 && !method.eq_ignore_ascii_case("HEAD")
        {
            method = "GET".to_string();
            body.clear();
        }
    }
}

fn build_client(target: &ResolvedTarget, timeout: Duration) -> Result<HttpClient, String> {
    let resolve_map = target
        .addresses
        .iter()
        .fold(ResolveMap::new(), |map, address| {
            map.add(&target.host, target.port, *address)
        });
    HttpClient::builder()
        .timeout(timeout)
        .redirect_policy(RedirectPolicy::None)
        .proxy(None::<isahc::http::Uri>)
        .version_negotiation(VersionNegotiation::http11())
        .dns_resolve(resolve_map)
        .build()
        .map_err(|e| format!("client error: {e}"))
}

async fn send_with_retries(
    client: &HttpClient,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    retries: u32,
) -> Result<isahc::http::Response<AsyncBody>, String> {
    let mut last_err = None;
    for attempt in 0..=retries {
        let request = build_request(url, USER_AGENT, method, headers, body.to_vec())?;
        match client.send_async(request).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let is_cf_challenge = status == 403
                    && response
                        .headers()
                        .get(CF_MITIGATED)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.contains(CF_CHALLENGE));
                if is_cf_challenge && method.eq_ignore_ascii_case("GET") {
                    let request =
                        build_request(url, FALLBACK_USER_AGENT, method, headers, body.to_vec())?;
                    match client.send_async(request).await {
                        Ok(response) => return Ok(response),
                        Err(error) => last_err = Some(format!("request failed: {error}")),
                    }
                } else if status >= 500 && attempt < retries {
                    last_err = Some(format!("HTTP {status}"));
                } else {
                    return Ok(response);
                }
            }
            Err(error) => last_err = Some(format!("request failed: {error}")),
        }
    }
    Err(last_err.unwrap_or_else(|| "request failed".to_string()))
}

async fn read_response(
    mut response: isahc::http::Response<AsyncBody>,
    max_bytes: usize,
) -> Result<ResponseData, String> {
    let status = response.status().as_u16();

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(len) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && len > max_bytes
    {
        return Err(format!("response too large: {len} bytes"));
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("read error: {e}"))?;

    if bytes.len() > max_bytes {
        return Err(format!("response too large: {} bytes", bytes.len()));
    }

    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ResponseData {
        body,
        status,
        content_type,
    })
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn resolve_redirect(original: &Url, current: &Url, location: &str) -> Result<Url, String> {
    validate_url_characters(location)?;
    validate_raw_url_path(location)?;
    if location.starts_with("//") || has_url_scheme(location) {
        validate_raw_authority(location)?;
    }

    let mut target = current
        .join(location)
        .map_err(|e| format!("invalid redirect URL: {e}"))?;
    validate_parsed_url(&target)?;
    validate_url_percent_encoding(target.path())?;
    target.set_fragment(None);

    if target.scheme() != "https" {
        return Err("blocked HTTPS downgrade redirect".to_string());
    }
    if !same_origin(original, &target) {
        return Err("blocked redirect outside original origin".to_string());
    }
    if !path_is_in_subtree(original.path(), target.path()) {
        return Err("blocked redirect outside original path subtree".to_string());
    }
    Ok(target)
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn path_is_in_subtree(root: &str, candidate: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|suffix| root.ends_with('/') || suffix.starts_with('/'))
}

fn resolve_target(url: &Url) -> Result<ResolvedTarget, String> {
    let host = url.host().ok_or_else(|| "URL has no host".to_string())?;
    let host_name = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no known port".to_string())?;
    let resolved = match host {
        Host::Ipv4(address) => Ok(vec![IpAddr::V4(address)]),
        Host::Ipv6(address) => Ok(vec![IpAddr::V6(address)]),
        Host::Domain(domain) => (domain, port)
            .to_socket_addrs()
            .map(|addresses| addresses.map(|address| address.ip()).collect())
            .map_err(|error| error.to_string()),
    };
    let addresses = validate_resolved_addresses(&host_name, resolved)?;
    Ok(ResolvedTarget {
        host: host_name,
        port,
        addresses,
    })
}

fn validate_resolved_addresses(
    host: &str,
    resolved: Result<Vec<IpAddr>, String>,
) -> Result<Vec<IpAddr>, String> {
    let mut addresses = resolved.map_err(|error| format!("cannot resolve {host}: {error}"))?;
    if addresses.is_empty() {
        return Err(format!("cannot resolve {host}: no addresses returned"));
    }
    addresses.sort_unstable();
    addresses.dedup();
    if let Some(address) = addresses.iter().copied().find(|address| risky_ip(*address)) {
        return Err(format!(
            "blocked: {host} resolves to private/reserved address {address}"
        ));
    }
    Ok(addresses)
}

fn risky_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, ..] = address.octets();
            first == 0
                || address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || first >= 240
                || first == 100 && (64..=127).contains(&second)
                || first == 192 && second == 0
                || first == 192 && second == 88
                || first == 198 && (18..=19).contains(&second)
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return risky_ip(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            address.is_unspecified()
                || address.is_loopback()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.is_multicast()
                || segments[0] & 0xe000 != 0x2000
                || segments[0] == 0x2001
                    && matches!(segments[1], 0x0000 | 0x0002 | 0x0010..=0x001f | 0x0db8)
                || segments[0] == 0x2002
        }
    }
}

fn validate_and_upgrade_url(value: &str) -> Result<Url, String> {
    validate_url_characters(value)?;
    validate_raw_url_path(value)?;
    validate_raw_authority(value)?;
    let mut url = Url::parse(value).map_err(|e| format!("invalid URL: {e}"))?;
    validate_parsed_url(&url)?;
    validate_url_percent_encoding(url.path())?;
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|()| "cannot upgrade URL to HTTPS".to_string())?;
    }
    url.set_fragment(None);
    Ok(url)
}

fn validate_url_characters(value: &str) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        return Err("URL contains control characters".to_string());
    }
    if value.contains('\\') {
        return Err("URL contains backslashes".to_string());
    }
    Ok(())
}

fn validate_url_percent_encoding(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let high = *bytes
            .get(index + 1)
            .ok_or_else(|| "URL contains invalid percent encoding".to_string())?;
        let low = *bytes
            .get(index + 2)
            .ok_or_else(|| "URL contains invalid percent encoding".to_string())?;
        let decoded = (hex_value(high)? << 4) | hex_value(low)?;
        if decoded == b'/'
            || decoded == b'\\'
            || decoded == b'.'
            || decoded == b'%'
            || decoded <= 0x1f
            || decoded == 0x7f
            || decoded.is_ascii_alphanumeric()
            || matches!(decoded, b'-' | b'_' | b'~')
        {
            return Err("URL contains ambiguous percent encoding".to_string());
        }
        index += 3;
    }
    Ok(())
}

fn validate_raw_url_path(value: &str) -> Result<(), String> {
    let without_query = value.split(['?', '#']).next().unwrap_or_default();
    let path = if let Some(rest) = without_query.strip_prefix("//") {
        rest.find('/').map_or("", |index| &rest[index..])
    } else if let Some((_, rest)) = without_query.split_once("://") {
        rest.find('/').map_or("", |index| &rest[index..])
    } else {
        without_query
    };
    validate_url_percent_encoding(path)
}

fn hex_value(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("URL contains invalid percent encoding".to_string()),
    }
}

fn validate_raw_authority(value: &str) -> Result<(), String> {
    let authority = raw_authority(value).ok_or_else(|| "URL has no host".to_string())?;
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    if authority.contains('@') {
        return Err("URL userinfo is not allowed".to_string());
    }
    Ok(())
}

fn raw_authority(value: &str) -> Option<&str> {
    let rest = if let Some(rest) = value.strip_prefix("//") {
        rest
    } else {
        value.split_once(':')?.1.strip_prefix("//")?
    };
    Some(rest.split(['/', '?', '#']).next().unwrap_or_default())
}

fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn validate_parsed_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "URL scheme must be http or https, got: {}",
            url.scheme()
        ));
    }
    if url.host().is_none() {
        return Err("URL has no host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL userinfo is not allowed".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use test_case::test_case;

    #[test_case("https://example.com", "https://example.com/" ; "https_normalized")]
    #[test_case("http://EXAMPLE.com/a/../b#fragment", "https://example.com/b" ; "http_upgraded_and_fragment_stripped")]
    fn validate_and_upgrade_url_valid(input: &str, expected: &str) {
        assert_eq!(validate_and_upgrade_url(input).unwrap().as_str(), expected);
    }

    #[test_case("ftp://example.com" ; "unsupported_scheme")]
    #[test_case("example.com" ; "bare_domain")]
    #[test_case("https://" ; "missing_host")]
    #[test_case("https:///path" ; "empty_authority")]
    #[test_case("https://example.com\\path" ; "backslash")]
    #[test_case("https://example.com/\npath" ; "control_character")]
    #[test_case("https://example.com/safe/%2f..%2fadmin" ; "encoded_separator")]
    #[test_case("https://example.com/safe/%2e%2e/admin" ; "encoded_dot_segment")]
    #[test_case("https://example.com/safe/%zz" ; "invalid_percent_encoding")]
    fn validate_and_upgrade_url_invalid(input: &str) {
        assert!(validate_and_upgrade_url(input).is_err());
    }

    #[test_case("https://user@example.com" ; "username")]
    #[test_case("https://user:password@example.com" ; "username_and_password")]
    #[test_case("https://:password@example.com" ; "empty_username")]
    #[test_case("https://@example.com" ; "empty_userinfo")]
    fn validate_and_upgrade_url_rejects_userinfo(input: &str) {
        assert!(validate_and_upgrade_url(input).is_err());
    }

    #[test_case("8.8.8.8", false ; "public_ipv4")]
    #[test_case("0.0.0.1", true ; "this_network")]
    #[test_case("10.0.0.1", true ; "private_ipv4")]
    #[test_case("100.64.0.1", true ; "shared_address_space")]
    #[test_case("127.0.0.1", true ; "loopback_ipv4")]
    #[test_case("169.254.169.254", true ; "link_local_ipv4")]
    #[test_case("192.0.0.1", true ; "ietf_protocol_assignments")]
    #[test_case("192.0.2.1", true ; "documentation_ipv4")]
    #[test_case("192.88.99.1", true ; "deprecated_relay")]
    #[test_case("198.18.0.1", true ; "benchmarking")]
    #[test_case("224.0.0.1", true ; "multicast_ipv4")]
    #[test_case("240.0.0.1", true ; "reserved_ipv4")]
    #[test_case("255.255.255.255", true ; "broadcast_ipv4")]
    #[test_case("2606:4700:4700::1111", false ; "public_ipv6")]
    #[test_case("::", true ; "unspecified_ipv6")]
    #[test_case("::1", true ; "loopback_ipv6")]
    #[test_case("::ffff:127.0.0.1", true ; "mapped_loopback")]
    #[test_case("fd00::1", true ; "unique_local_ipv6")]
    #[test_case("fe80::1", true ; "link_local_ipv6")]
    #[test_case("ff02::1", true ; "multicast_ipv6")]
    #[test_case("2001:db8::1", true ; "documentation_ipv6")]
    #[test_case("2002::1", true ; "six_to_four")]
    fn risky_ip_cases(input: &str, expected: bool) {
        assert_eq!(risky_ip(input.parse().unwrap()), expected);
    }

    #[test]
    fn resolved_addresses_fail_closed_and_deduplicate() {
        assert!(validate_resolved_addresses("example.com", Err("DNS failed".into())).is_err());
        assert!(validate_resolved_addresses("example.com", Ok(Vec::new())).is_err());

        let first = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let second = IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111));
        let addresses =
            validate_resolved_addresses("example.com", Ok(vec![second, first, first])).unwrap();
        assert_eq!(addresses, vec![first, second]);
    }

    #[test]
    fn resolved_addresses_reject_if_any_result_is_risky() {
        let public = "8.8.8.8".parse().unwrap();
        let private = "127.0.0.1".parse().unwrap();
        assert!(validate_resolved_addresses("example.com", Ok(vec![public, private])).is_err());
    }

    #[test_case("/docs/page", true ; "same_origin_subtree")]
    #[test_case("https://EXAMPLE.com:443/docs/page", true ; "normalized_origin")]
    #[test_case("https://other.example/docs/page", false ; "different_host")]
    #[test_case("https://example.com:444/docs/page", false ; "different_port")]
    #[test_case("/other", false ; "outside_path")]
    #[test_case("/docs-other", false ; "path_prefix_is_not_subtree")]
    #[test_case("/docs/%2F..%2Fadmin", false ; "encoded_separator")]
    #[test_case("http://example.com/docs/page", false ; "https_downgrade")]
    fn redirect_origin_and_path_policy(location: &str, expected: bool) {
        let original = validate_and_upgrade_url("https://example.com/docs/").unwrap();
        assert_eq!(
            resolve_redirect(&original, &original, location).is_ok(),
            expected
        );
    }

    #[test_case("child", Ok("https://example.com/docs/child") ; "relative_child")]
    #[test_case("?page=2#fragment", Ok("https://example.com/docs/start?page=2") ; "query_and_fragment")]
    #[test_case("../outside", Err(()) ; "relative_parent_outside_subtree")]
    fn relative_redirects(location: &str, expected: Result<&str, ()>) {
        let original = validate_and_upgrade_url("https://example.com/docs/").unwrap();
        let current = validate_and_upgrade_url("https://example.com/docs/start").unwrap();
        let result = resolve_redirect(&original, &current, location);
        match expected {
            Ok(expected) => assert_eq!(result.unwrap().as_str(), expected),
            Err(()) => assert!(result.is_err()),
        }
    }

    #[test]
    fn build_request_get_no_opts() {
        let req = build_request("https://example.com", "agent", "GET", &[], vec![]).unwrap();
        assert_eq!(req.method(), "GET");
        assert_eq!(req.body().len(), Some(0));
        assert_eq!(req.headers()["User-Agent"], "agent");
    }

    #[test]
    fn build_request_post_with_body_and_headers() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let req = build_request(
            "https://example.com",
            "agent",
            "POST",
            &headers,
            b"hello world".to_vec(),
        )
        .unwrap();
        assert_eq!(req.method(), "POST");
        assert_eq!(req.body().len(), Some(b"hello world".len() as u64));
        assert_eq!(req.headers()["Content-Type"], "application/json");
    }

    #[test]
    fn build_request_multiple_headers() {
        let headers = vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("X-Custom".to_string(), "foo".to_string()),
        ];
        let req = build_request("https://example.com", "agent", "GET", &headers, vec![]).unwrap();
        assert_eq!(req.headers()["Accept"], "text/html");
        assert_eq!(req.headers()["X-Custom"], "foo");
    }

    #[test]
    fn build_request_invalid_uri_errors() {
        let result = build_request("not a valid uri \x00", "agent", "GET", &[], vec![]);
        assert!(result.is_err());
    }

    #[test_case(r#"net.request("https://127.0.0.1")"# ; "ssrf_blocked")]
    #[test_case(r#"net.request("ftp://x")"# ; "invalid_url")]
    fn lua_request_error_returns_nil_and_message(expr: &str) {
        let lua = Lua::new();
        let net = create_net_table(&lua, &PluginPermissions::trusted()).unwrap();
        lua.globals().set("net", net).unwrap();
        let (is_nil, has_err): (bool, bool) = lua
            .load(format!(
                "local r, err = {expr}; return r == nil, err ~= nil"
            ))
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn extract_params_defaults_no_opts() {
        let params = extract_request_params("https://example.com", None).unwrap();
        assert_eq!(params.url.as_str(), "https://example.com/");
        assert_eq!(params.method, "GET");
        assert!(params.headers.is_empty());
        assert!(params.body.is_empty());
        assert_eq!(params.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(params.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(params.retries, MAX_RETRIES);
    }

    #[test]
    fn extract_params_timeout_clamped_to_max() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("timeout", MAX_TIMEOUT_SECS + 100).unwrap();
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn extract_params_post_with_body() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("method", "POST").unwrap();
        opts.set("body", r#"{"key":"val"}"#).unwrap();
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.method, "POST");
        assert_eq!(params.body, br#"{"key":"val"}"#);
    }

    #[test]
    fn extract_params_http_upgraded_to_https() {
        let params = extract_request_params("http://example.com", None).unwrap();
        assert_eq!(params.url.as_str(), "https://example.com/");
    }

    #[test]
    fn extract_params_headers_collected() {
        let lua = Lua::new();
        let headers = lua.create_table().unwrap();
        headers.set("Authorization", "Bearer tok").unwrap();
        headers.set("Accept", "text/html").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("headers", headers).unwrap();
        let params = extract_request_params("https://example.com", Some(&opts)).unwrap();
        assert_eq!(params.headers.len(), 2);
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer tok")
        );
        assert!(
            params
                .headers
                .iter()
                .any(|(k, v)| k == "Accept" && v == "text/html")
        );
    }
}
