use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use url::Url;
use workcell_net::{BoundedResponse, FetchOptions, NetError, RetryPolicy};

use crate::budget::ResolutionBudget;
use crate::resolver::{ResolveSourceIconOptions, SourceIconError, SourceIconResolver};

const USER_AGENT: &str = "Workcell-SourceIcons/0.1";
const MAX_REDIRECTS: usize = 2;

pub(crate) enum FetchOutcome {
    Response(Box<BoundedResponse>),
    DefinitiveFailure,
    TransientFailure,
}

pub(crate) struct FetchSpec<'a> {
    pub(crate) url: &'a Url,
    pub(crate) timeout: Duration,
    pub(crate) max_body_bytes: usize,
    pub(crate) accept: &'a str,
    pub(crate) range: Option<&'a str>,
}

impl SourceIconResolver {
    pub(crate) async fn fetch_page_html(
        &self,
        page_url: &Url,
        options: &ResolveSourceIconOptions,
        budget: &ResolutionBudget,
    ) -> Result<Option<String>, SourceIconError> {
        let FetchOutcome::Response(response) = self
            .fetch(
                FetchSpec {
                    url: page_url,
                    timeout: options.timeout,
                    max_body_bytes: options.max_html_bytes,
                    accept: "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
                    range: None,
                },
                options,
                budget,
            )
            .await?
        else {
            return Ok(None);
        };
        if !response.status.is_success() {
            return Ok(None);
        }
        let content_type = normalized_content_type(&response.headers);
        if content_type.is_some_and(|value| !value.contains("html")) {
            return Ok(None);
        }
        if response.body.is_empty() {
            Ok(None)
        } else {
            // Icon declarations normally occur in `<head>`, so a bounded HTML
            // prefix remains useful even when the page stream was truncated.
            Ok(Some(String::from_utf8_lossy(&response.body).into_owned()))
        }
    }

    pub(crate) async fn fetch(
        &self,
        spec: FetchSpec<'_>,
        options: &ResolveSourceIconOptions,
        budget: &ResolutionBudget,
    ) -> Result<FetchOutcome, SourceIconError> {
        let Some(timeout) = budget.begin_request(spec.timeout, &options.cancellation)? else {
            return Ok(FetchOutcome::TransientFailure);
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_str(spec.accept).expect("static Accept value"),
        );
        headers.insert(
            http::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        if let Some(range) = spec
            .range
            .and_then(|value| HeaderValue::from_str(value).ok())
        {
            headers.insert(http::header::RANGE, range);
        }
        let result = self
            .client
            .get_url(
                spec.url.clone(),
                FetchOptions {
                    timeout,
                    max_redirects: MAX_REDIRECTS,
                    max_body_bytes: spec.max_body_bytes,
                    headers,
                    retry: bounded_retry(&options.retry),
                    cancellation: options.cancellation.clone(),
                },
            )
            .await;
        match result {
            Ok(response) => Ok(FetchOutcome::Response(Box::new(response))),
            Err(NetError::Cancelled) => Err(SourceIconError::Cancelled),
            Err(NetError::Policy(_)) => Ok(FetchOutcome::DefinitiveFailure),
            Err(_) => Ok(FetchOutcome::TransientFailure),
        }
    }
}

pub(crate) fn definitive_http_failure(status: http::StatusCode) -> bool {
    status.is_client_error()
        && !matches!(
            status,
            http::StatusCode::REQUEST_TIMEOUT
                | http::StatusCode::TOO_EARLY
                | http::StatusCode::TOO_MANY_REQUESTS
        )
}

pub(crate) fn normalized_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn bounded_retry(retry: &RetryPolicy) -> RetryPolicy {
    let mut retry = retry.clone();
    retry.max_retries = retry.max_retries.min(3);
    retry
}
