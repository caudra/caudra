mod content;
mod icons;
mod input;
mod output;
mod pdf_response;
mod sites;

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_net::PrivateAllowlist;
use workcell_source_icons::{ResolvedSourceIcon, SourceIconError};

use crate::WebToolDependencies;
use crate::blocking;
use crate::dependencies::{WebHttpError, WebHttpRequest, WebHttpResponse};
use crate::html::add_title_context;
use crate::types::{WebfetchFormat, WebfetchOutput};

pub(crate) use input::{NormalizedWebfetchInput, normalize_input};
pub(crate) use output::utf8_prefix;
use sites::{Render, RenderContext, SiteAttempt, SitePage, SiteRenderError};

const MIB: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 5 * MIB;
pub(super) const MAX_PDF_RESPONSE_BYTES: usize = 6 * MIB;
pub(super) const MAX_REDIRECTS: usize = 5;
const REQUEST_FAILED: &str = "webfetch request failed.";
const RETRY_AFTER_HEADERS: &[&str] = &["retry-after", "x-ratelimit-reset"];

#[derive(Debug, thiserror::Error)]
pub enum WebfetchError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Tool invocation was aborted.")]
    Aborted,
    #[error("{0}")]
    TimedOut(String),
    /// Network safety rules or the outbound proxy refused the target.
    #[error("{0}")]
    Denied(String),
    /// Network safety rules refused a private or special-use target, which a
    /// private allowlist could admit. `url` is the refused target, and
    /// `redirected` says a redirect led to it.
    #[error("{message}")]
    PrivateTarget {
        message: String,
        url: Url,
        redirected: bool,
    },
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Operation(String),
}

impl WebfetchError {
    /// The symbolic token a caller branches on, so it never has to parse the
    /// message to tell a refused target from a slow or broken one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid_input",
            Self::Aborted => "cancelled",
            Self::TimedOut(_) => "timed_out",
            Self::Denied(_) | Self::PrivateTarget { .. } => "web_request_denied",
            Self::NotFound(_) => "not_found",
            Self::Operation(_) => "operation_failed",
        }
    }

    fn timed_out(timeout_seconds: u64) -> Self {
        Self::TimedOut(format!("Request timed out after {timeout_seconds} seconds"))
    }
}

pub(crate) struct WebfetchExecution {
    pub output: WebfetchOutput,
    pub model_text: String,
}

pub(crate) async fn execute(
    mut input: input::NormalizedWebfetchInput,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<WebfetchExecution, WebfetchError> {
    let timeout = Duration::from_secs(input.timeout_seconds);
    // Webfetch timeout is one total deadline for network transfer and primary
    // HTML/PDF parsing, shared by every attempt a site rule makes. Optional
    // icon decoration is skipped at that deadline.
    let deadline = Instant::now() + timeout;
    // One read of the list covers the whole fetch, so an entry removed after
    // the call was prepared or approved is already refused here.
    let private = dependencies.private_allowlist.current();
    private
        .validate_url(input.policy, &input.url)
        .map_err(|error| input::policy_error(error, &input.requested, false))?;
    let (attempts, site_icon) = match input.site.take() {
        Some(site) => (site.attempts, site.icon),
        None => (
            vec![SiteAttempt::direct(
                input.url.clone(),
                input.headers.clone(),
            )],
            None,
        ),
    };
    let last = attempts.len().saturating_sub(1);
    for (index, attempt) in attempts.into_iter().enumerate() {
        let budget = if index == 0 {
            timeout
        } else {
            deadline.saturating_duration_since(Instant::now())
        };
        let response = send(
            &input,
            &attempt,
            Arc::clone(&private),
            budget,
            &cancellation,
            dependencies,
        )
        .await?;
        if !response.status.is_success() {
            if index < last && attempt.fallback_on.contains(&response.status) {
                continue;
            }
            return Err(status_error(response.status, &response.headers));
        }
        let Some(render) = attempt.render else {
            return page(input, response, dependencies, cancellation, deadline).await;
        };
        match read_site(render, &input, &response, &cancellation, deadline).await? {
            Ok(read) => {
                return site_output(
                    &input,
                    response,
                    read,
                    site_icon,
                    dependencies,
                    cancellation,
                    deadline,
                )
                .await;
            }
            Err(_) if index < last => {}
            Err(error) => {
                return Err(WebfetchError::Operation(format!(
                    "webfetch could not read {}: {error}",
                    response.final_url
                )));
            }
        }
    }
    Err(WebfetchError::Operation(REQUEST_FAILED.to_owned()))
}

async fn send(
    input: &input::NormalizedWebfetchInput,
    attempt: &SiteAttempt,
    private: Arc<PrivateAllowlist>,
    timeout: Duration,
    cancellation: &CancellationToken,
    dependencies: &WebToolDependencies,
) -> Result<WebHttpResponse, WebfetchError> {
    if timeout.is_zero() {
        return Err(WebfetchError::timed_out(input.timeout_seconds));
    }
    let named = if attempt.url == input.url {
        &input.requested
    } else {
        &attempt.url
    };
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: input.request_kind,
            method: input.method.clone(),
            url: attempt.url.clone(),
            headers: attempt.headers.clone(),
            body: None,
            timeout,
            max_redirects: input.max_redirects,
            // Octet-stream responses need the PDF allowance before their
            // signature can be inspected.
            max_body_bytes: input.max_body_bytes,
            cancellation: cancellation.clone(),
            private: Arc::clone(&private),
        })
        .await
        .map_err(|error| map_http_error(error, input.timeout_seconds, named))?;
    private
        .validate_url(input.policy, &response.final_url)
        .map_err(|error| {
            input::policy_error(
                error,
                &response.final_url,
                response.final_url != attempt.url,
            )
        })?;
    Ok(response)
}

fn status_error(status: StatusCode, headers: &HeaderMap) -> WebfetchError {
    let mut message = format!(
        "webfetch returned {} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or_default()
    );
    if status == StatusCode::TOO_MANY_REQUESTS
        && let Some(seconds) = retry_after_seconds(headers)
    {
        message.push_str(&format!("; retry after {seconds}s"));
    }
    match status {
        StatusCode::NOT_FOUND | StatusCode::GONE => WebfetchError::NotFound(message),
        _ => WebfetchError::Operation(message),
    }
}

/// Seconds until a rate limit lifts, from `Retry-After` in its delta form or
/// the `X-RateLimit-Reset` some sites send instead.
fn retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    RETRY_AFTER_HEADERS.iter().find_map(|name| {
        let seconds = headers
            .get(*name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?;
        (seconds.is_finite() && seconds >= 0.0).then(|| seconds.ceil() as u64)
    })
}

fn non_text_error(content_type: Option<&str>) -> WebfetchError {
    WebfetchError::Operation(format!(
        "webfetch cannot return non-text content type: {}",
        content_type.unwrap_or("unknown")
    ))
}

struct SiteRead {
    page: SitePage,
    content_type: Option<String>,
    source_truncated: bool,
}

/// Renders a site response off the async runtime. The outer error ends the
/// fetch; the inner one lets the next attempt try.
async fn read_site(
    render: Render,
    input: &input::NormalizedWebfetchInput,
    response: &WebHttpResponse,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Result<SiteRead, SiteRenderError>, WebfetchError> {
    let content_type = content::normalized_content_type(&response.headers);
    if content_type
        .as_deref()
        .is_some_and(|value| !content::is_text_like(value))
    {
        return Err(non_text_error(content_type.as_deref()));
    }
    let readable = response.body.len().min(MAX_RESPONSE_BYTES);
    let text = content::decode_text(&response.body[..readable], &response.headers, false);
    let context = RenderContext {
        final_url: response.final_url.clone(),
        markdown: input.format == WebfetchFormat::Markdown,
    };
    let page = blocking::run_until(deadline, cancellation, move || render(&text, &context))
        .await
        .map_err(|error| content::blocking_error(error, input.timeout_seconds))?;
    Ok(page.map(|page| SiteRead {
        page,
        content_type,
        source_truncated: response.truncated || readable < response.body.len(),
    }))
}

async fn site_output(
    input: &input::NormalizedWebfetchInput,
    response: WebHttpResponse,
    read: SiteRead,
    site_icon: Option<&'static str>,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WebfetchExecution, WebfetchError> {
    let SiteRead {
        page,
        content_type,
        source_truncated,
    } = read;
    let text = add_title_context(
        &page.body,
        page.title.as_deref(),
        input.format == WebfetchFormat::Markdown,
    );
    let source_cut = source_truncated.then(source_cut_notice);
    let bounded = output::truncate_model_output(&text, source_cut.as_deref());
    // A declared icon keeps the resolver from fetching the page again, which a
    // rate-limited site would count against the next fetch.
    let icon = match site_icon {
        Some(href) => {
            resolve_icon(
                dependencies,
                input.url.as_str(),
                Some(format!(r#"<link rel="icon" href="{href}">"#)),
                cancellation,
                deadline,
            )
            .await?
        }
        None => None,
    };
    let output = WebfetchOutput {
        kind: "webfetch",
        url: input.url.to_string(),
        final_url: Some(response.final_url.to_string()),
        content_type,
        format: input.format,
        pdf_mode: None,
        status: response.status.as_u16(),
        title: page.title,
        output: bounded.text.clone(),
        summary_input: None,
        truncated: source_truncated || bounded.truncated,
        pdf_attachment: None,
        page_count: None,
        pdf_fallback_reason: None,
        extraction_method: Some(page.method),
        extraction_low_signal: None,
        icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
        icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
    };
    Ok(WebfetchExecution {
        model_text: bounded.text,
        output,
    })
}

fn source_cut_notice() -> String {
    format!("the first {} MiB of the response", MAX_RESPONSE_BYTES / MIB)
}

async fn resolve_icon(
    dependencies: &WebToolDependencies,
    page_url: &str,
    html: Option<String>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<Option<ResolvedSourceIcon>, WebfetchError> {
    icons::resolve(dependencies, page_url, html, cancellation, deadline)
        .await
        .or_else(|error| match error {
            SourceIconError::Cancelled => Err(WebfetchError::Aborted),
            _ => Ok(None),
        })
}

/// The ordinary pipeline: PDF, HTML, or text, chosen by the response.
async fn page(
    input: input::NormalizedWebfetchInput,
    response: WebHttpResponse,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WebfetchExecution, WebfetchError> {
    let content_type = content::normalized_content_type(&response.headers);
    // Re-apply the wire bound here so an injected transport cannot bypass the
    // same memory/output invariant enforced by the production net client.
    let bounded_length = response.body.len().min(MAX_PDF_RESPONSE_BYTES);
    let body_was_over_limit = response.body.len() > MAX_PDF_RESPONSE_BYTES;
    let body = response.body.slice(..bounded_length);
    if content::is_pdf(content_type.as_deref())
        || content::should_probe_pdf(content_type.as_deref())
    {
        return pdf_response::execute(
            pdf_response::PdfResponse {
                request: input,
                status: response.status,
                final_url: response.final_url,
                bytes: body.to_vec(),
                body_truncated: response.truncated || body_was_over_limit,
                content_type,
            },
            dependencies,
            cancellation,
            deadline,
        )
        .await;
    }
    if content_type
        .as_deref()
        .is_some_and(|value| !content::is_text_like(value))
    {
        return Err(non_text_error(content_type.as_deref()));
    }

    let text_body_truncated = body.len() > MAX_RESPONSE_BYTES;
    let body = content::decode_text(
        &body[..body.len().min(MAX_RESPONSE_BYTES)],
        &response.headers,
        content::is_html(content_type.as_deref()),
    );
    let formatted = content::format(
        body.clone(),
        input.format,
        content_type.as_deref(),
        response.final_url.as_str(),
        cancellation.clone(),
        deadline,
        input.timeout_seconds,
    )
    .await?;
    let source_cut = (response.truncated || text_body_truncated).then(source_cut_notice);
    let bounded = output::truncate_model_output(&formatted.output, source_cut.as_deref());
    let summary_input = formatted
        .summary_input
        .as_deref()
        .map(output::truncate_summary_input);
    let icon = resolve_icon(
        dependencies,
        response.final_url.as_str(),
        content::is_html(content_type.as_deref()).then_some(body),
        cancellation,
        deadline,
    )
    .await?;
    let output = WebfetchOutput {
        kind: "webfetch",
        url: input.url.to_string(),
        final_url: Some(response.final_url.to_string()),
        content_type,
        format: input.format,
        pdf_mode: None,
        status: response.status.as_u16(),
        title: formatted.title,
        output: bounded.text.clone(),
        summary_input,
        truncated: response.truncated
            || text_body_truncated
            || formatted.truncated
            || bounded.truncated,
        pdf_attachment: None,
        page_count: None,
        pdf_fallback_reason: None,
        extraction_method: formatted.extraction_method,
        extraction_low_signal: formatted.extraction_low_signal,
        icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
        icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
    };
    Ok(WebfetchExecution {
        model_text: bounded.text,
        output,
    })
}

/// `target` is the URL a refusal names when no redirect led elsewhere.
fn map_http_error(error: WebHttpError, timeout_seconds: u64, target: &Url) -> WebfetchError {
    match error {
        WebHttpError::Cancelled => WebfetchError::Aborted,
        WebHttpError::Timeout => WebfetchError::timed_out(timeout_seconds),
        WebHttpError::Rejected(message) => WebfetchError::Denied(input::blocked(message)),
        WebHttpError::NonPublicTarget { message, redirect } => WebfetchError::PrivateTarget {
            message: input::blocked(message),
            redirected: redirect.is_some(),
            url: redirect.unwrap_or_else(|| target.clone()),
        },
        WebHttpError::ProxyRejected => WebfetchError::Denied(
            "The outbound proxy refused this request; the target is not permitted.".to_owned(),
        ),
        WebHttpError::RedirectRejected | WebHttpError::RequestFailed => {
            WebfetchError::Operation(REQUEST_FAILED.to_owned())
        }
    }
}
