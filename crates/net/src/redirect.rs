use std::net::IpAddr;

use http::{HeaderMap, Method, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::body::read_bounded_body;
use crate::deadline::{remaining, try_run_until};
use crate::{
    BoundedResponse, HttpClient, NetError, ProxyRoute, RedirectScope, RequestSpec,
    TransportRequest, TransportResponse, TransportRoute, UrlPolicyError,
};

const MAX_REDIRECTS: usize = 20;

impl HttpClient {
    pub(crate) async fn fetch_redirect_chain(
        &self,
        request: &RequestSpec,
        deadline: Instant,
    ) -> Result<BoundedResponse, NetError> {
        let options = &request.options;
        let mut url = request.url.clone();
        let mut headers = options.headers.clone();
        let redirect_limit = options.max_redirects.min(MAX_REDIRECTS);
        for redirect_count in 0..=redirect_limit {
            let response = self.execute_hop(request, &url, &headers, deadline).await?;
            let Some(location) = followed_location(request, &response, &url)? else {
                let body = read_bounded_body(
                    response,
                    options.max_body_bytes,
                    deadline,
                    &options.cancellation,
                )
                .await?;
                return Ok(BoundedResponse {
                    status: body.status,
                    headers: body.headers,
                    url,
                    body: body.bytes,
                    truncated: body.truncated,
                });
            };
            if redirect_count >= redirect_limit {
                return Err(NetError::Redirect("redirect limit exceeded".to_owned()));
            }
            let next = self.policy.parse_url(location, Some(&url))?;
            if !same_origin(&url, &next) {
                // A blacklist cannot enumerate custom credential headers such
                // as X-API-Key. Rebuild from the small set a redirected GET
                // needs so an untrusted origin never receives caller secrets.
                headers = cross_origin_headers(&headers);
            }
            // Dropping the response drops its stream. Redirect bodies are not
            // drained because they are attacker-controlled and may be unbounded.
            url = next;
        }
        Err(NetError::Redirect("redirect limit exceeded".to_owned()))
    }

    async fn execute_hop(
        &self,
        request: &RequestSpec,
        url: &Url,
        headers: &HeaderMap,
        deadline: Instant,
    ) -> Result<TransportResponse, NetError> {
        let cancellation = &request.options.cancellation;
        // Every hop gets a fresh policy and DNS check. Validating only the first
        // URL would allow an otherwise public endpoint to redirect into a LAN.
        self.policy.validate_url(url)?;
        // The route is decided per hop, not per operation: a redirect onto a
        // bypassed host is dialled directly and pinned, and a redirect off one
        // goes to the proxy.
        let route = match self.proxy.route(url) {
            ProxyRoute::Proxy(endpoint) => TransportRoute::Proxy { endpoint },
            ProxyRoute::Direct => TransportRoute::Direct {
                resolved_addresses: self.resolve_target(url, deadline, cancellation).await?,
            },
        };
        let hop = TransportRequest {
            method: request.method.clone(),
            url: url.clone(),
            headers: headers.clone(),
            body: request.body.clone(),
            route,
            timeout: remaining(deadline)?,
        };
        try_run_until(deadline, cancellation, self.transport.execute(hop)).await
    }

    async fn resolve_target(
        &self,
        url: &Url,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Vec<IpAddr>, NetError> {
        match url.host().ok_or(UrlPolicyError::MissingHost)? {
            Host::Ipv4(address) => {
                let address = IpAddr::V4(address);
                self.policy.validate_ip(address)?;
                Ok(vec![address])
            }
            Host::Ipv6(address) => {
                let address = IpAddr::V6(address);
                self.policy.validate_ip(address)?;
                Ok(vec![address])
            }
            Host::Domain(hostname) => {
                let addresses =
                    try_run_until(deadline, cancellation, self.resolver.resolve(hostname)).await?;
                if addresses.is_empty() {
                    return Err(NetError::EmptyDnsAnswer(hostname.to_owned()));
                }
                // Reject mixed public/private answers rather than allowing the
                // connector to choose a policy-violating address.
                for address in &addresses {
                    self.policy.validate_ip(*address)?;
                }
                Ok(addresses)
            }
        }
    }
}

/// The target the chain follows from `response`, or `None` to end it there.
///
/// A body-less GET has nothing to replay, so it follows any redirect status,
/// leaving the origin of the hop that answered only under
/// [`RedirectScope::AnyOrigin`]. Anything else follows only a 307 or 308, the
/// redirects that ask for the same request again, and only within that origin.
fn followed_location<'a>(
    request: &RequestSpec,
    response: &'a TransportResponse,
    url: &Url,
) -> Result<Option<&'a str>, NetError> {
    if request.method != Method::GET || request.body.is_some() {
        return Ok(replayable_redirect_location(response, url));
    }
    let location = redirect_location(response)?;
    Ok(match request.redirects {
        RedirectScope::AnyOrigin => location,
        RedirectScope::SameOrigin => location.filter(|location| !leaves_origin(url, location)),
    })
}

/// The target of any redirect. A redirect without one is an error.
fn redirect_location(response: &TransportResponse) -> Result<Option<&str>, NetError> {
    if !is_redirect(response.status) {
        return Ok(None);
    }
    location(&response.headers)
        .map(Some)
        .ok_or_else(|| NetError::Redirect("missing Location header".to_owned()))
}

/// The target of a 307 or 308 that stays on the current origin. Every other
/// response, including such a redirect without a usable target, ends the chain.
fn replayable_redirect_location<'a>(response: &'a TransportResponse, url: &Url) -> Option<&'a str> {
    if !matches!(
        response.status,
        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
    ) {
        return None;
    }
    location(&response.headers)
        .filter(|location| url.join(location).is_ok_and(|next| same_origin(url, &next)))
}

/// Whether `location` resolves against `url` to another origin. A target that
/// does not resolve is left for the redirect parse to reject, as it is under
/// any scope.
fn leaves_origin(url: &Url, location: &str) -> bool {
    url.join(location)
        .is_ok_and(|next| !same_origin(url, &next))
}

fn location(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn cross_origin_headers(headers: &HeaderMap) -> HeaderMap {
    const SAFE_HEADERS: &[http::header::HeaderName] = &[
        http::header::ACCEPT,
        http::header::ACCEPT_LANGUAGE,
        http::header::CACHE_CONTROL,
        http::header::PRAGMA,
        http::header::RANGE,
        http::header::USER_AGENT,
    ];
    let mut safe = HeaderMap::new();
    for name in SAFE_HEADERS {
        for value in headers.get_all(name) {
            safe.append(name, value.clone());
        }
    }
    safe
}
