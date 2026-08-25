use std::net::IpAddr;

use http::{HeaderMap, Method, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::body::read_bounded_body;
use crate::deadline::{remaining, run_until};
use crate::{
    BoundedResponse, FetchOptions, HttpClient, NetError, TransportRequest, TransportResponse,
    UrlPolicyError,
};

const MAX_REDIRECTS: usize = 20;

impl HttpClient {
    pub(crate) async fn fetch_redirect_chain(
        &self,
        initial_url: Url,
        options: &FetchOptions,
        deadline: Instant,
    ) -> Result<BoundedResponse, NetError> {
        let mut url = initial_url;
        let mut headers = options.headers.clone();
        let redirect_limit = options.max_redirects.min(MAX_REDIRECTS);
        for redirect_count in 0..=redirect_limit {
            let response = self
                .execute_hop(&url, &headers, deadline, &options.cancellation)
                .await?;
            if !is_redirect(response.status) {
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
            }

            let location = response
                .headers
                .get(http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| NetError::Redirect("missing Location header".to_owned()))?;
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
        url: &Url,
        headers: &HeaderMap,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<TransportResponse, NetError> {
        // Every hop gets a fresh policy and DNS check. Validating only the first
        // URL would allow an otherwise public endpoint to redirect into a LAN.
        self.policy.validate_url(url)?;
        let addresses = self.resolve_target(url, deadline, cancellation).await?;
        let request = TransportRequest {
            method: Method::GET,
            url: url.clone(),
            headers: headers.clone(),
            resolved_addresses: addresses,
            timeout: remaining(deadline)?,
        };
        Ok(run_until(deadline, cancellation, self.transport.execute(request)).await??)
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
                    run_until(deadline, cancellation, self.resolver.resolve(hostname)).await??;
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
