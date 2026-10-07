//! `http()` for automation firings. On the actor, [`prepare`] checks a request against the
//! script's header and the session's network policy before any I/O, reading only the
//! environment variables the header declares, and builds the [`HttpCall`] an [`HttpClient`]
//! performs off the actor. A [`Redactor`] hides the request's secrets in everything the host shows
//! of it: the response the firing receives, the journal, the trace, the mirror and error messages.

use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::mem;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::time::Duration;

use caudra_automation::host::{
    Failure, FailureKind, HostError, HostResult, HttpMethod, HttpPayload, HttpRequest,
    HttpResponse, HttpTarget, MAX_REQUEST_BODY_BYTES, MAX_RESPONSE_BYTES,
};
use caudra_automation::meta::{AutomationMeta, private_host};
use caudra_config::global_env_value;
use caudra_providers::user_agent;
use futures_lite::future;
use serde_json::Value;
use thiserror::Error;
use url::Url;

use super::clock::Wake;
use super::host::Stop;

const HTTP: &str = "http";
const HTTPS: &str = "https";
const AUTHORIZATION: &str = "Authorization";
const CONTENT_TYPE: &str = "Content-Type";
const USER_AGENT: &str = "User-Agent";
const JSON_MEDIA_TYPE: &str = "application/json";
const BEARER_SCHEME: &str = "Bearer";
/// The `tchar`s of RFC 9110 besides ASCII letters and digits.
const TOKEN_SYMBOLS: &[u8] = b"!#$%&'*+-.^_`|~";
const TAB: u8 = b'\t';
const VISIBLE_ASCII: RangeInclusive<u8> = b' '..=b'~';
pub const UNDECLARED_ORIGIN: &str = "http() may reach only the origins in meta.network, not";
pub const UNDECLARED_SECRET: &str = "http() may read only the variables in meta.secrets, not";
pub const PRIVATE_TARGET: &str = "http() may reach a loopback or private host only with \
     [automations] allow_private_network, not";
pub const NOT_AN_HTTP_URL: &str = "is not an http or https URL";
pub const UNSET_SECRET: &str = "is unset or empty";
pub const NOT_UNICODE: &str = "does not hold valid Unicode";
pub const INVALID_HEADER_NAME: &str = "is not a valid header name";
pub const INVALID_HEADER_VALUE: &str = "must hold visible ASCII characters, spaces and tabs only";
pub const DUPLICATE_HEADER: &str = "is set more than once";
pub const BEARER_CONFLICT: &str =
    "bearer_env sets the Authorization header, so headers and secret_headers must not";
pub const BODY_TOO_LARGE: &str = "the request body is over its limit of";
pub const TIMED_OUT: &str = "the request ran past its timeout of";
const SECRET_OPEN: &str = "${";
const SECRET_CLOSE: &str = "}";

pub type HttpFuture = Pin<Box<dyn Future<Output = Result<HttpAnswer, HttpError>> + Send + 'static>>;

/// Performs the `http()` requests of one session's automations.
pub trait HttpClient: Send + Sync {
    /// Returns at once: the future performs `call`, once and without retries, and ends within
    /// `call.timeout`. Dropping the future cancels the request, at any point.
    fn send(&self, call: HttpCall) -> HttpFuture;
}

/// A request the host checked and resolved. It carries secrets, so nothing logs or stores it.
pub struct HttpCall {
    pub method: HttpMethod,
    /// The final URL, with the script's `query` appended.
    pub url: Url,
    /// In sending order, secret values and `Authorization` included.
    pub headers: Vec<(String, String)>,
    /// At most [`MAX_REQUEST_BODY_BYTES`].
    pub body: Option<Vec<u8>>,
    /// The script's timeout, cut to what the firing's wall time has left.
    pub timeout: Duration,
    /// `[automations] allow_private_network`. Without it a name that resolves to a loopback or
    /// private address is refused, as [`HttpErrorKind::Refused`].
    pub allow_private_network: bool,
    /// The most body bytes to read. Past them the body is cut and [`HttpAnswer::truncated`] set.
    pub max_response_bytes: usize,
}

/// What the server answered, whatever the status.
pub struct HttpAnswer {
    pub status: u16,
    pub body: Vec<u8>,
    /// The server sent more than [`HttpCall::max_response_bytes`].
    pub truncated: bool,
}

/// Why no response arrived. `message` names neither the URL nor a secret; the host still hides
/// the ones it knows.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct HttpError {
    pub kind: HttpErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorKind {
    /// The request ran past [`HttpCall::timeout`].
    Timeout,
    /// Connecting, sending, or reading the response failed.
    Transport,
    /// Policy refused the target, such as a name that resolved to a private address.
    Refused,
    /// The client would not send the request as given, such as one setting a header the client
    /// reserves for itself. Nothing was sent.
    InvalidArgument,
}

/// A checked request, and what the host may show of it.
pub(super) struct Prepared {
    pub(super) call: HttpCall,
    /// `scheme://host[:port]`, all anyone sees of a `url_env` target.
    pub(super) origin: String,
    /// The environment variables the request read, by name.
    pub(super) variables: Vec<String>,
    pub(super) redactor: Redactor,
}

/// The secrets a request carries, longest first so a secret that holds another is hidden whole,
/// each with the text shown in its place.
#[derive(Default)]
pub(super) struct Redactor {
    hidden: Vec<(String, String)>,
}

impl HttpError {
    pub fn new(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The failure the firing may catch, with the request's secrets hidden.
    fn into_failure(self, redactor: &Redactor) -> Failure {
        let kind = match self.kind {
            HttpErrorKind::Timeout => FailureKind::Timeout,
            HttpErrorKind::Transport => FailureKind::Transport,
            HttpErrorKind::Refused => FailureKind::Refused,
            HttpErrorKind::InvalidArgument => FailureKind::InvalidArgument,
        };
        Failure::new(kind, redactor.text(&self.message))
    }
}

impl HttpAnswer {
    /// The firing's view: the body cut to [`MAX_RESPONSE_BYTES`] and decoded lossily, and its
    /// JSON unless it was cut.
    fn into_response(self) -> HttpResponse {
        let Self {
            status,
            mut body,
            truncated,
        } = self;
        let cut = truncated || body.len() > MAX_RESPONSE_BYTES;
        body.truncate(MAX_RESPONSE_BYTES);
        let mut body = String::from_utf8(body)
            .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned());
        body.truncate(body.floor_char_boundary(MAX_RESPONSE_BYTES));
        let json = if cut {
            None
        } else {
            serde_json::from_str(&body).ok()
        };
        HttpResponse { status, body, json }
    }
}

impl Redactor {
    fn hide(&mut self, secret: &str, shown: &str) {
        if secret.is_empty() || secret == shown {
            return;
        }
        let at = self
            .hidden
            .partition_point(|(hidden, _)| hidden.len() >= secret.len());
        self.hidden
            .insert(at, (secret.to_owned(), shown.to_owned()));
    }

    /// `text` with every secret replaced, scanned once per secret, and borrowed when it holds none.
    pub(super) fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.hidden
            .iter()
            .fold(Cow::Borrowed(text), |text, (secret, shown)| {
                replaced(text, secret, shown)
            })
    }

    /// The response as the firing, its state, its trace and the journal see it: the request's
    /// secrets replaced in `body` and in the strings and keys of `json`, parsed before from the
    /// body as the server sent it.
    fn response(&self, mut response: HttpResponse) -> HttpResponse {
        if self.hidden.is_empty() {
            return response;
        }
        response.body = self.owned(response.body);
        if let Some(json) = &mut response.json {
            self.redact(json);
        }
        response
    }

    fn redact(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.owned(mem::take(text)),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact(item)),
            Value::Object(entries) => {
                entries.values_mut().for_each(|item| self.redact(item));
                if entries.keys().any(|key| self.hides(key)) {
                    *entries = mem::take(entries)
                        .into_iter()
                        .map(|(key, item)| (self.owned(key), item))
                        .collect();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// `text` itself, unless it holds a secret.
    fn owned(&self, text: String) -> String {
        let replaced = match self.text(&text) {
            Cow::Owned(replaced) => Some(replaced),
            Cow::Borrowed(_) => None,
        };
        replaced.unwrap_or(text)
    }

    fn hides(&self, text: &str) -> bool {
        self.hidden
            .iter()
            .any(|(secret, _)| text.contains(secret.as_str()))
    }
}

/// `text` with every `secret` in it replaced by `shown`, in one pass.
fn replaced<'a>(text: Cow<'a, str>, secret: &str, shown: &str) -> Cow<'a, str> {
    let Some(first) = text.find(secret) else {
        return text;
    };
    let mut replaced = String::with_capacity(text.len());
    let mut copied = 0;
    for (found, _) in text[first..].match_indices(secret) {
        let at = first + found;
        replaced.push_str(&text[copied..at]);
        replaced.push_str(shown);
        copied = at + secret.len();
    }
    replaced.push_str(&text[copied..]);
    Cow::Owned(replaced)
}

/// Checks `request` and resolves its secrets before any I/O, every stop ahead of any failure the
/// firing may catch, so a policy violation is never caught. `time_left` is what remains of the
/// firing's wall time.
///
/// 1. Stops the firing: a variable outside `meta.secrets`.
/// 2. Stops the firing: a URL, or a `url_env` value, that is not an http(s) URL, an origin
///    outside `meta.network`, and a loopback or private host without `allow_private_network`.
///    Reading the `url_env` value may fail first, as in 4.
/// 3. Fails with `invalid_argument`: an invalid header name or value, a header set twice,
///    `bearer_env` beside an `Authorization` header, and a body over [`MAX_REQUEST_BODY_BYTES`].
/// 4. Fails with `refused` when a variable is unset or empty, naming it but never showing its
///    value, and with `invalid_argument` when its value is not Unicode or not a header value.
pub(super) fn prepare(
    request: &HttpRequest,
    meta: &AutomationMeta,
    allow_private_network: bool,
    time_left: Duration,
) -> Result<Prepared, HostError> {
    let variables = variables(request);
    if let Some(name) = variables.iter().find(|name| !meta.secrets.contains(name)) {
        return Err(HostError::Refused(format!("{UNDECLARED_SECRET} {name}")));
    }
    let mut redactor = Redactor::default();
    let mut url = match &request.target {
        HttpTarget::Url(url) => {
            http_url(url).ok_or_else(|| HostError::Refused(format!("{url:?} {NOT_AN_HTTP_URL}")))?
        }
        HttpTarget::UrlEnv(name) => secret_url(name, &mut redactor)?,
    };
    let origin = url.origin().ascii_serialization();
    if !meta.network.contains(&origin) {
        return Err(HostError::Refused(format!("{UNDECLARED_ORIGIN} {origin}")));
    }
    if !allow_private_network && url.host().is_some_and(private_host) {
        return Err(HostError::Refused(format!("{PRIVATE_TARGET} {origin}")));
    }
    check_headers(request)?;
    let body = body(request.payload.as_ref())?;
    let headers = resolve_headers(request, &mut redactor)?;
    if !request.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&request.query);
        if matches!(request.target, HttpTarget::UrlEnv(_)) {
            redactor.hide(url.as_str(), &origin);
        }
    }
    Ok(Prepared {
        call: HttpCall {
            method: request.method,
            url,
            headers,
            body,
            timeout: request.timeout.min(time_left),
            allow_private_network,
            max_response_bytes: MAX_RESPONSE_BYTES,
        },
        origin,
        variables,
        redactor,
    })
}

/// Waits for the client's answer as the firing sees it, the request's secrets hidden, unless the
/// firing must stop or `deadline` passes first, however the client keeps time. Returning drops
/// the client's future, which cancels a request in flight; a deadline that already passed never
/// polls it.
pub(super) async fn perform(
    sending: HttpFuture,
    stop: &Stop,
    deadline: Wake,
    timeout: Duration,
    redactor: &Redactor,
) -> HostResult<HttpResponse> {
    let timed_out = async move {
        deadline.await;
        Err(HttpError::new(
            HttpErrorKind::Timeout,
            format!("{TIMED_OUT} {timeout:?}"),
        ))
    };
    let answered = async {
        future::or(timed_out, sending)
            .await
            .map(|answer| redactor.response(answer.into_response()))
            .map_err(|error| error.into_failure(redactor).into())
    };
    future::or(
        async { Err(HostError::Interrupted(stop.raised().await)) },
        answered,
    )
    .await
}

/// Header names are tokens, set once in any case, literal values are visible ASCII, and
/// `Authorization` comes from `bearer_env` or from the headers, not both.
fn check_headers(request: &HttpRequest) -> Result<(), HostError> {
    let mut names = HashSet::new();
    for name in request.headers.keys().chain(request.secret_headers.keys()) {
        if !is_token(name) {
            return Err(invalid(format!("{name:?} {INVALID_HEADER_NAME}")));
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(invalid(format!("header {name} {DUPLICATE_HEADER}")));
        }
        if request.bearer_env.is_some() && name.eq_ignore_ascii_case(AUTHORIZATION) {
            return Err(invalid(BEARER_CONFLICT));
        }
    }
    match request
        .headers
        .iter()
        .find(|(_, value)| !is_header_value(value))
    {
        Some((name, _)) => Err(invalid(format!("header {name} {INVALID_HEADER_VALUE}"))),
        None => Ok(()),
    }
}

fn body(payload: Option<&HttpPayload>) -> Result<Option<Vec<u8>>, HostError> {
    let body = match payload {
        None => return Ok(None),
        Some(HttpPayload::Json(value)) => {
            Cow::Owned(serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))?)
        }
        Some(HttpPayload::Body(text)) => Cow::Borrowed(text.as_bytes()),
    };
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(invalid(format!(
            "{BODY_TOO_LARGE} {MAX_REQUEST_BODY_BYTES} bytes"
        )));
    }
    Ok(Some(body.into_owned()))
}

/// The variables `request` names: its `url_env`, its `bearer_env` and its secret headers'.
fn variables(request: &HttpRequest) -> Vec<String> {
    let url_env = match &request.target {
        HttpTarget::UrlEnv(name) => Some(name),
        HttpTarget::Url(_) => None,
    };
    url_env
        .into_iter()
        .chain(&request.bearer_env)
        .chain(request.secret_headers.values())
        .cloned()
        .collect()
}

/// The URL `name` holds, hidden behind its origin wherever the host shows it.
fn secret_url(name: &str, redactor: &mut Redactor) -> Result<Url, HostError> {
    let value = secret(name)?;
    let url = http_url(&value)
        .ok_or_else(|| HostError::Refused(format!("the value of {name} {NOT_AN_HTTP_URL}")))?;
    let origin = url.origin().ascii_serialization();
    redactor.hide(&value, &origin);
    redactor.hide(url.as_str(), &origin);
    Ok(url)
}

/// The literal headers, the secret ones, `Authorization` from `bearer_env`, `Content-Type` for a
/// JSON body the script gave none, and Caudra's `User-Agent` unless the script set one.
fn resolve_headers(
    request: &HttpRequest,
    redactor: &mut Redactor,
) -> Result<Vec<(String, String)>, HostError> {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    for (name, variable) in &request.secret_headers {
        let value = secret(variable)?;
        if !is_header_value(&value) {
            return Err(invalid(format!(
                "header {name} from {variable} {INVALID_HEADER_VALUE}"
            )));
        }
        redactor.hide(&value, &shown_secret(variable));
        headers.push((name.clone(), value));
    }
    if let Some(variable) = &request.bearer_env {
        let token = secret(variable)?;
        if !is_header_value(&token) {
            return Err(invalid(format!("{variable} {INVALID_HEADER_VALUE}")));
        }
        redactor.hide(&token, &shown_secret(variable));
        headers.push((AUTHORIZATION.to_owned(), format!("{BEARER_SCHEME} {token}")));
    }
    if matches!(request.payload, Some(HttpPayload::Json(_))) && !has_header(&headers, CONTENT_TYPE)
    {
        headers.push((CONTENT_TYPE.to_owned(), JSON_MEDIA_TYPE.to_owned()));
    }
    if !has_header(&headers, USER_AGENT) {
        headers.push((USER_AGENT.to_owned(), user_agent().to_owned()));
    }
    Ok(headers)
}

fn has_header(headers: &[(String, String)], wanted: &str) -> bool {
    headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(wanted))
}

/// The value of a variable [`prepare`] found in `meta.secrets`, read as the global config reads
/// one, so a project's `.env` cannot plant it.
fn secret(name: &str) -> Result<String, HostError> {
    match global_env_value(name) {
        Ok(Some(value)) if !value.is_empty() => Ok(value),
        Ok(_) => Err(Failure::new(FailureKind::Refused, format!("{name} {UNSET_SECRET}")).into()),
        Err(_) => Err(invalid(format!("{name} {NOT_UNICODE}"))),
    }
}

fn http_url(text: &str) -> Option<Url> {
    Url::parse(text)
        .ok()
        .filter(|url| matches!(url.scheme(), HTTP | HTTPS))
}

fn shown_secret(variable: &str) -> String {
    format!("{SECRET_OPEN}{variable}{SECRET_CLOSE}")
}

fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_SYMBOLS.contains(&byte))
}

fn is_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == TAB || VISIBLE_ASCII.contains(&byte))
}

fn invalid(message: impl Into<String>) -> HostError {
    Failure::new(FailureKind::InvalidArgument, message).into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::env;

    use caudra_automation::host::MAX_HTTP_TIMEOUT;
    use caudra_automation::meta::parse_meta;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const API_ORIGIN: &str = "https://api.example.com";
    const API_URL: &str = "https://api.example.com/v1/items";
    const LOOPBACK_ORIGIN: &str = "http://127.0.0.1:8080";
    const LOOPBACK_URL: &str = "http://127.0.0.1:8080/health";
    const ELSEWHERE_ORIGIN: &str = "https://elsewhere.example.com";
    const ELSEWHERE_URL: &str = "https://elsewhere.example.com/hook";
    const KEY_HEADER: &str = "X-Key";
    const KEY_HEADER_LOWER: &str = "x-key";
    const CONTENT_TYPE_LOWER: &str = "content-type";
    const USER_AGENT_LOWER: &str = "user-agent";
    const USER_AGENT_UPPER: &str = "USER-AGENT";
    const NOTE_HEADER: &str = "X-Note";
    const BAD_HEADER_NAME: &str = "X Note";
    const NOTE: &str = "plain";
    const SCRIPT_AGENT: &str = "probe/1.0";
    const KEY_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_KEY";
    const KEY: &str = "unit-key-0123";
    const BAD_KEY_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_BAD_KEY";
    const BAD_KEY: &str = "naïve-key";
    const AGENT_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_AGENT";
    const SECRET_AGENT: &str = "probe-secret/2.0";
    const UNDECLARED_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_UNDECLARED";
    const URL_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_URL";
    const ELSEWHERE_URL_VAR: &str = "CAUDRA_TEST_HTTP_UNIT_ELSEWHERE_URL";
    const NOT_HTTP: &str = "ftp://files.example.com/drop-4711";
    const FILLER: &str = "x";
    const NON_ASCII: &str = "café";
    const TEXT_TYPE: &str = "text/plain";
    const STATUS: u16 = 200;
    const JSON_BODY: &str = r#"{"ok":true}"#;
    const PLAIN_BODY: &str = "pong";
    const INVALID_UTF8: &[u8] = b"o\xffk";
    const LOSSY: &str = "o\u{fffd}k";
    const WIDE_CHAR: &str = "é";
    const SECRET_URL: &str = "https://hooks.example.com/services/hook-4711";
    const HOOK_ORIGIN: &str = "https://hooks.example.com";
    const TOKEN: &str = "hook-4711";
    const TOKEN_SHOWN: &str = "${HOOK_TOKEN}";
    const LEAK: &str = "the firing must not receive a secret";

    fn set_env(name: &str, value: &str) {
        // SAFETY: test-only variables with names no other test reads, always set to one value.
        unsafe { env::set_var(name, value) };
    }

    fn meta() -> AutomationMeta {
        parse_meta(&format!(
            r#"let meta = #{{ name: "probe", description: "Probe", triggers: [#{{ kind: "armed" }}], network: ["{API_ORIGIN}", "{LOOPBACK_ORIGIN}"], secrets: ["{KEY_VAR}", "{BAD_KEY_VAR}", "{AGENT_VAR}", "{URL_VAR}", "{ELSEWHERE_URL_VAR}"] }};"#
        ))
        .unwrap()
    }

    fn request(
        target: HttpTarget,
        headers: &[(&str, &str)],
        secret_headers: &[(&str, &str)],
        payload: Option<HttpPayload>,
    ) -> HttpRequest {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        HttpRequest {
            method: HttpMethod::Post,
            target,
            query: BTreeMap::new(),
            headers: map(headers),
            bearer_env: None,
            secret_headers: map(secret_headers),
            payload,
            timeout: MAX_HTTP_TIMEOUT,
        }
    }

    fn api(headers: &[(&str, &str)], secret_headers: &[(&str, &str)]) -> HttpRequest {
        request(
            HttpTarget::Url(API_URL.to_owned()),
            headers,
            secret_headers,
            None,
        )
    }

    fn prepared(request: &HttpRequest) -> Result<Prepared, HostError> {
        prepare(request, &meta(), false, MAX_HTTP_TIMEOUT)
    }

    fn answer(body: &[u8], truncated: bool) -> HttpAnswer {
        HttpAnswer {
            status: STATUS,
            body: body.to_vec(),
            truncated,
        }
    }

    /// The values sent under `wanted`, in any case.
    fn sent<'a>(headers: &'a [(String, String)], wanted: &str) -> Vec<&'a str> {
        headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    #[test]
    fn a_content_type_the_script_set_wins_over_the_json_default() {
        let request = request(
            HttpTarget::Url(API_URL.to_owned()),
            &[(CONTENT_TYPE_LOWER, TEXT_TYPE)],
            &[],
            Some(HttpPayload::Json(json!({ "ok": true }))),
        );

        let headers = prepared(&request).unwrap().call.headers;

        assert_eq!(sent(&headers, CONTENT_TYPE), [TEXT_TYPE]);
    }

    #[test_case(api(&[], &[]), user_agent(); "caudras_own_without_one")]
    #[test_case(api(&[(USER_AGENT_LOWER, SCRIPT_AGENT)], &[]), SCRIPT_AGENT; "the_scripts_in_another_case")]
    #[test_case(api(&[], &[(USER_AGENT_UPPER, AGENT_VAR)]), SECRET_AGENT; "the_scripts_from_a_secret")]
    fn a_request_carries_one_user_agent(request: HttpRequest, expected: &str) {
        set_env(AGENT_VAR, SECRET_AGENT);

        let headers = prepared(&request).unwrap().call.headers;

        assert_eq!(sent(&headers, USER_AGENT), [expected]);
    }

    #[test_case(request(HttpTarget::UrlEnv(ELSEWHERE_URL_VAR.to_owned()), &[(BAD_HEADER_NAME, NOTE)], &[], None), format!("{UNDECLARED_ORIGIN} {ELSEWHERE_ORIGIN}"); "an_undeclared_url_env_origin_beside_an_invalid_header")]
    #[test_case(request(HttpTarget::Url(LOOPBACK_URL.to_owned()), &[], &[], Some(HttpPayload::Body(FILLER.repeat(MAX_REQUEST_BODY_BYTES + 1)))), format!("{PRIVATE_TARGET} {LOOPBACK_ORIGIN}"); "a_private_target_beside_an_oversize_body")]
    #[test_case(api(&[(BAD_HEADER_NAME, NOTE)], &[(KEY_HEADER, UNDECLARED_VAR)]), format!("{UNDECLARED_SECRET} {UNDECLARED_VAR}"); "an_undeclared_secret_header_variable_beside_an_invalid_header")]
    fn a_policy_violation_stops_ahead_of_any_failure_the_firing_may_catch(
        request: HttpRequest,
        message: String,
    ) {
        set_env(ELSEWHERE_URL_VAR, ELSEWHERE_URL);

        let refused = prepared(&request).err();

        assert_eq!(refused, Some(HostError::Refused(message)));
    }

    #[test_case(api(&[(KEY_HEADER_LOWER, KEY)], &[(KEY_HEADER, KEY_VAR)]), format!("header {KEY_HEADER} {DUPLICATE_HEADER}"); "a_name_twice_in_another_case")]
    #[test_case(api(&[(NOTE_HEADER, NON_ASCII)], &[]), format!("header {NOTE_HEADER} {INVALID_HEADER_VALUE}"); "a_literal_value_beyond_ascii")]
    #[test_case(api(&[], &[(KEY_HEADER, BAD_KEY_VAR)]), format!("header {KEY_HEADER} from {BAD_KEY_VAR} {INVALID_HEADER_VALUE}"); "a_secret_value_beyond_ascii")]
    fn an_invalid_header_fails_with_invalid_argument(request: HttpRequest, message: String) {
        set_env(KEY_VAR, KEY);
        set_env(BAD_KEY_VAR, BAD_KEY);

        let refused = prepared(&request).err();

        assert_eq!(
            refused,
            Some(HostError::Failure(Failure::new(
                FailureKind::InvalidArgument,
                message
            )))
        );
    }

    #[test]
    fn a_url_env_that_holds_no_http_url_stops_without_showing_its_value() {
        set_env(URL_VAR, NOT_HTTP);
        let request = request(HttpTarget::UrlEnv(URL_VAR.to_owned()), &[], &[], None);

        let refused = prepared(&request).err();

        assert_eq!(
            refused,
            Some(HostError::Refused(format!(
                "the value of {URL_VAR} {NOT_AN_HTTP_URL}"
            )))
        );
    }

    #[test_case(answer(JSON_BODY.as_bytes(), false) => (JSON_BODY.to_owned(), Some(json!({ "ok": true }))); "json")]
    #[test_case(answer(PLAIN_BODY.as_bytes(), false) => (PLAIN_BODY.to_owned(), None); "not_json")]
    #[test_case(answer(JSON_BODY.as_bytes(), true) => (JSON_BODY.to_owned(), None); "cut_by_the_client")]
    #[test_case(answer(INVALID_UTF8, false) => (LOSSY.to_owned(), None); "invalid_utf8")]
    fn an_answer_reaches_the_firing_with_its_status_body_and_json(
        answer: HttpAnswer,
    ) -> (String, Option<Value>) {
        let response = answer.into_response();
        assert_eq!(response.status, STATUS);
        (response.body, response.json)
    }

    #[test]
    fn a_body_past_the_cap_is_cut_on_a_char_boundary_and_never_parsed() {
        let body = format!("\"{}\"", WIDE_CHAR.repeat(MAX_RESPONSE_BYTES));

        let response = answer(body.as_bytes(), false).into_response();

        assert!(response.body.len() <= MAX_RESPONSE_BYTES);
        assert!(response.body.ends_with(WIDE_CHAR));
        assert_eq!(response.json, None);
    }

    #[test]
    fn the_redactor_hides_a_secret_whole_before_a_secret_inside_it() {
        let mut redactor = Redactor::default();
        redactor.hide(TOKEN, TOKEN_SHOWN);
        redactor.hide(SECRET_URL, HOOK_ORIGIN);

        let text = format!("{SECRET_URL} sent {TOKEN}, then {TOKEN}");

        let shown = redactor.text(&text);

        assert_eq!(
            shown,
            format!("{HOOK_ORIGIN} sent {TOKEN_SHOWN}, then {TOKEN_SHOWN}")
        );
    }

    #[test]
    fn the_firing_receives_a_response_without_the_secrets_in_its_body_and_json() {
        let mut redactor = Redactor::default();
        redactor.hide(TOKEN, TOKEN_SHOWN);
        let body = json!({ "echo": TOKEN, TOKEN: [TOKEN] }).to_string();

        let response = redactor.response(answer(body.as_bytes(), false).into_response());

        assert_eq!(response.body, body.replace(TOKEN, TOKEN_SHOWN), "{LEAK}");
        assert_eq!(
            response.json,
            Some(json!({ "echo": TOKEN_SHOWN, TOKEN_SHOWN: [TOKEN_SHOWN] })),
            "{LEAK}"
        );
    }
}
