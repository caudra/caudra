use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use caudra_config::ProxyRoute;
use caudra_storage::StateDir;
use caudra_storage::auth::load_mcp_auth;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, future};
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use isahc::http::header::{
    ACCEPT, AUTHORIZATION, AsHeaderName, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
};
use isahc::http::{Method, Request, StatusCode};
use isahc::net::dns::ResolveMap;
use isahc::{AsyncBody, HttpClient, Response as HttpResponse};
use serde::Serialize;
use serde_json::Value;
use smol::Timer;
use tracing::{debug, info, warn};
use url::{Host, Url};

use super::call::{CallScope, CancelOnDrop, lock, with_progress_token};
use super::error::McpError;
use super::inbound::{Inbound, Route};
use super::listen::{self, backoff};
use super::message::{self, Message, Request as ServerRequest, Response, ResponseBody};
use super::oauth;
use super::protocol::{
    INITIALIZE_METHOD, INITIALIZED_NOTIFICATION, JsonRpcNotification, JsonRpcRequest,
    LISTEN_METHOD, PROMPTS_GET_METHOD, RESOURCES_READ_METHOD, ServerCapabilities,
    TOOLS_CALL_METHOD, TOOLS_LIST_METHOD, initialize_params, is_modern_error,
};
use super::transport::{BoxFuture, McpTransport};

const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const METHOD_HEADER: &str = "mcp-method";
const NAME_HEADER: &str = "mcp-name";
const PARAM_HEADER_PREFIX: &str = "mcp-param-";
const SENTINEL_PREFIX: &str = "=?base64?";
const SENTINEL_SUFFIX: &str = "?=";
const X_MCP_HEADER: &str = "x-mcp-header";
const PROPERTIES_KEY: &str = "properties";
const TYPE_KEY: &str = "type";
const NULL_TYPE: &str = "null";
const HEADER_TYPES: &[&str] = &["integer", "string", "boolean"];
const TOKEN_SYMBOLS: &[u8] = b"!#$%&'*+-.^_`|~";
const TOOLS_KEY: &str = "tools";
const NAME_KEY: &str = "name";
const URI_KEY: &str = "uri";
const ARGUMENTS_KEY: &str = "arguments";
const INPUT_SCHEMA_KEY: &str = "inputSchema";
const INVALID_HEADER_NAME: &str = "x-mcp-header is not a header name token:";
const NOT_PRIMITIVE: &str =
    "x-mcp-header marks a property that is not an integer, string or boolean:";
const DUPLICATE_HEADER: &str = "x-mcp-header names one header twice:";
const UNREACHABLE_HEADER: &str = "x-mcp-header sits outside a chain of properties";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";
const WWW_AUTHENTICATE_HEADER: &str = "www-authenticate";
const PROTOCOL_VERSION_KEY: &str = "protocolVersion";
pub(super) const CT_JSON: &str = "application/json";
const CT_SSE: &str = "text/event-stream";
pub(super) const ACCEPT_VALUE: &str = "application/json, text/event-stream";
const SSE_DATA: &str = "data";
const SSE_ID: &str = "id";
const SSE_RETRY: &str = "retry";
const SSE_COMMENT: char = ':';
const NOTIFICATION_REJECTED: &str = "notification rejected";
const LISTEN_REFUSED: &str = "server refused the GET stream";

pub struct HttpTransport {
    shared: Arc<Shared>,
    /// The subscription, or a legacy server's GET stream, open once the
    /// server declared a list that can change. Dropping the task closes it.
    listener: SyncMutex<Option<smol::Task<()>>>,
}

struct Shared {
    name: Arc<str>,
    url: String,
    client: HttpClient,
    headers: HashMap<String, String>,
    resolved: Vec<IpAddr>,
    auth: Mutex<Option<String>>,
    storage: Option<StateDir>,
    negotiated: Mutex<Negotiated>,
    /// Held while a lost session is replaced, so concurrent calls that all
    /// saw it go make one new session between them.
    reinitializing: Mutex<()>,
    next_id: AtomicU64,
    timeout: Duration,
    inbound: Inbound,
    /// What each listed tool of a modern server wants mirrored into headers.
    param_headers: SyncMutex<HashMap<String, Vec<ParamHeader>>>,
}

/// An argument a tool wants mirrored into an `Mcp-Param-{Name}` header,
/// found by its chain of `properties` keys.
#[derive(Debug, Clone, PartialEq)]
struct ParamHeader {
    name: HeaderName,
    path: Vec<String>,
}

impl ParamHeader {
    /// Nothing for an argument that is absent or null.
    fn mirror(&self, arguments: &Value) -> Option<(HeaderName, HeaderValue)> {
        let value = self
            .path
            .iter()
            .try_fold(arguments, |value, key| value.get(key))?;
        let value = match value {
            Value::String(text) => header_value(text),
            Value::Number(number) => header_value(&number.to_string()),
            Value::Bool(flag) => header_value(&flag.to_string()),
            _ => None,
        }?;
        Some((self.name.clone(), value))
    }
}

/// The server picks both during initialize and the spec wants them echoed as
/// headers on every later request, so one lock keeps them in sync.
#[derive(Clone, Default)]
struct Negotiated {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl HttpTransport {
    pub fn new(
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
        resolved: &[IpAddr],
        timeout: Duration,
        storage: Option<StateDir>,
    ) -> Result<Self, McpError> {
        let parsed = Url::parse(url);
        let builder = parsed
            .as_ref()
            .map_or_else(|_| ProxyRoute::direct(), caudra_config::proxy_route)
            .apply(HttpClient::builder())
            // A trusted public endpoint must not redirect MCP traffic or
            // credentials into a private network target that was never reviewed.
            .redirect_policy(RedirectPolicy::None)
            // The workspace enables curl's http2 feature for OTLP over gRPC,
            // which would otherwise flip this transport to h2 over TLS. Its
            // streaming responses are tuned for HTTP/1.1, so pin it.
            .version_negotiation(VersionNegotiation::http11())
            // Only connecting is bounded here: a call's idle timeout, which
            // progress extends, and the GET stream outlive any fixed total.
            .connect_timeout(timeout);
        let builder = match (parsed, resolved.is_empty()) {
            (Ok(url), false) => match (url.host(), url.port_or_known_default()) {
                (Some(Host::Domain(host)), Some(port)) => {
                    let mappings = resolved.iter().fold(ResolveMap::new(), |map, address| {
                        map.add(host, port, *address)
                    });
                    builder.dns_resolve(mappings)
                }
                _ => builder,
            },
            _ => builder,
        };
        let client = builder
            .build()
            .map_err(|e: isahc::Error| McpError::StartFailed {
                server: name.into(),
                reason: e.to_string(),
            })?;

        let mut headers = headers.clone();
        let auth = headers
            .keys()
            .find(|k| k.eq_ignore_ascii_case(AUTHORIZATION.as_str()))
            .cloned()
            .and_then(|k| headers.remove(&k))
            .or_else(|| {
                let tokens = load_mcp_auth(storage.as_ref()?, name, url)?.tokens?;
                Some(format!("Bearer {}", tokens.access))
            });

        let name: Arc<str> = Arc::from(name);
        Ok(Self {
            shared: Arc::new(Shared {
                inbound: Inbound::new(Arc::clone(&name)),
                name,
                url: url.to_string(),
                client,
                headers,
                resolved: resolved.to_vec(),
                auth: Mutex::new(auth),
                storage,
                negotiated: Mutex::new(Negotiated::default()),
                reinitializing: Mutex::new(()),
                next_id: AtomicU64::new(1),
                timeout,
                param_headers: SyncMutex::default(),
            }),
            listener: SyncMutex::new(None),
        })
    }
}

impl Shared {
    fn server(&self) -> String {
        (*self.name).into()
    }

    fn invalid(&self, reason: impl ToString) -> McpError {
        McpError::InvalidResponse {
            server: self.server(),
            reason: reason.to_string(),
        }
    }

    fn write_failed(&self, error: isahc::Error) -> McpError {
        McpError::WriteFailed {
            server: self.server(),
            reason: error.to_string(),
        }
    }

    fn encode(&self, message: &impl Serialize) -> Result<Vec<u8>, McpError> {
        serde_json::to_vec(message).map_err(|e| self.invalid(e))
    }

    fn build_request(
        &self,
        method: Method,
        body: Vec<u8>,
        negotiated: &Negotiated,
        auth: Option<&str>,
        mirrored: &[(HeaderName, HeaderValue)],
    ) -> Result<Request<Vec<u8>>, McpError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(&self.url)
            .header(CONTENT_TYPE, CT_JSON)
            .header(ACCEPT, ACCEPT_VALUE);

        if let Some(sid) = &negotiated.session_id {
            builder = builder.header(SESSION_HEADER, sid);
        }

        if let Some(version) = &negotiated.protocol_version {
            builder = builder.header(PROTOCOL_HEADER, version);
        }

        for (name, value) in mirrored {
            builder = builder.header(name, value);
        }

        if let Some(auth) = auth {
            builder = builder.header(AUTHORIZATION, auth);
        }

        for (k, v) in &self.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder.body(body).map_err(|e| self.invalid(e))
    }

    fn request<'a>(
        self: &'a Arc<Self>,
        method: &'a str,
        params: Option<Value>,
        scope: &'a CallScope,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            let start = Instant::now();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let era = self.inbound.era();
            let params = era.with_meta(params, scope);
            let mirrored = era.modern().map_or_else(Vec::new, |version| {
                self.mirrored(version, method, params.as_ref())
            });
            let body = self.encode(&JsonRpcRequest::new(
                id,
                method,
                with_progress_token(method, params, id),
            ))?;
            let tracked = self.inbound.track(id, scope);
            // A modern server takes the dropped stream as the cancellation.
            let mut cancel = era.modern().is_none().then(|| {
                CancelOnDrop::new(method, id, |notification: Value| {
                    self.post_detached(notification);
                })
            });
            let mut result = scope
                .wait(
                    &self.name,
                    self.timeout,
                    self.exchange(method, id, body, &mirrored, tracked.route()),
                )
                .await;
            if let Some(cancel) = &mut cancel {
                cancel.settle(&result);
            }
            if method == TOOLS_LIST_METHOD
                && era.modern().is_some()
                && let Ok(page) = &mut result
            {
                self.screen_tools(page);
            }
            info!(server = %self.name, method, ok = result.is_ok(), duration_ms = start.elapsed().as_millis() as u64, "MCP HTTP request");
            result
        })
    }

    /// The modern headers that repeat the body for intermediaries: version
    /// and method always, the name a call targets, and the arguments its
    /// tool marked with `x-mcp-header`.
    fn mirrored(
        &self,
        version: &str,
        method: &str,
        params: Option<&Value>,
    ) -> Vec<(HeaderName, HeaderValue)> {
        let params = params.unwrap_or(&Value::Null);
        let target = match method {
            TOOLS_CALL_METHOD | PROMPTS_GET_METHOD => params[NAME_KEY].as_str(),
            RESOURCES_READ_METHOD => params[URI_KEY].as_str(),
            _ => None,
        };
        let mut headers: Vec<(HeaderName, HeaderValue)> = [
            (PROTOCOL_HEADER, Some(version)),
            (METHOD_HEADER, Some(method)),
            (NAME_HEADER, target),
        ]
        .into_iter()
        .filter_map(|(name, text)| Some((HeaderName::from_static(name), header_value(text?)?)))
        .collect();
        if method == TOOLS_CALL_METHOD
            && let Some(marked) =
                target.and_then(|tool| lock(&self.param_headers).get(tool).cloned())
        {
            headers.extend(
                marked
                    .iter()
                    .filter_map(|param| param.mirror(&params[ARGUMENTS_KEY])),
            );
        }
        headers
    }

    /// Drops each listed tool whose `x-mcp-header` marks break the rules,
    /// and keeps what the rest want mirrored.
    fn screen_tools(&self, page: &mut Value) {
        let Some(tools) = page.get_mut(TOOLS_KEY).and_then(Value::as_array_mut) else {
            return;
        };
        let mut marked = lock(&self.param_headers);
        tools.retain(|tool| {
            let name = tool[NAME_KEY].as_str().unwrap_or_default();
            match param_headers(&tool[INPUT_SCHEMA_KEY]) {
                Ok(headers) if headers.is_empty() => {
                    marked.remove(name);
                    true
                }
                Ok(headers) => {
                    marked.insert(name.to_owned(), headers);
                    true
                }
                Err(reason) => {
                    warn!(server = %self.name, tool = name, reason, "excluding an MCP tool whose x-mcp-header breaks the rules");
                    marked.remove(name);
                    false
                }
            }
        });
    }

    /// One POST and the response it opens, retried after a token refresh on
    /// 401 and, when a legacy session was lost, once more on a new session.
    async fn exchange(
        self: &Arc<Self>,
        method: &str,
        id: u64,
        body: Vec<u8>,
        mirrored: &[(HeaderName, HeaderValue)],
        route: &Route,
    ) -> Result<Value, McpError> {
        let mut auth = self.auth.lock().await.clone();
        let mut refreshed = false;
        let mut reinitialized = false;

        loop {
            let negotiated = self.negotiated.lock().await.clone();
            let request = self.build_request(
                Method::POST,
                body.clone(),
                &negotiated,
                auth.as_deref(),
                mirrored,
            )?;
            let response = self
                .client
                .send_async(request)
                .await
                .map_err(|e| self.write_failed(e))?;
            let status = response.status();

            if status == StatusCode::UNAUTHORIZED {
                if refreshed {
                    return Err(McpError::Refused {
                        server: self.server(),
                    });
                }
                if let Some(new_auth) = self.refreshed_auth(auth.as_deref()).await {
                    auth = Some(new_auth);
                    refreshed = true;
                    continue;
                }
            }

            if status == StatusCode::NOT_FOUND
                && method != INITIALIZE_METHOD
                && !reinitialized
                && let Some(lost) = negotiated.session_id.as_deref()
            {
                self.reinitialize(lost).await?;
                reinitialized = true;
                continue;
            }

            if !status.is_success() {
                return Err(self.http_error(response).await);
            }

            if self.inbound.era().modern().is_none()
                && let Some(session) = header(response.headers(), SESSION_HEADER)
            {
                self.negotiated.lock().await.session_id = Some(session.to_owned());
            }
            let sse =
                header(response.headers(), CONTENT_TYPE).is_some_and(|ct| ct.contains(CT_SSE));
            let result = self
                .receive(BufReader::new(response.into_body()), sse, id, route)
                .await;

            if method == INITIALIZE_METHOD
                && let Ok(result) = &result
                && let Some(version) = result.get(PROTOCOL_VERSION_KEY).and_then(Value::as_str)
            {
                self.negotiated.lock().await.protocol_version = Some(version.to_string());
            }
            return result;
        }
    }

    /// A modern server explains a refusal with a JSON-RPC error of its own,
    /// which then stands for the status it came with.
    async fn http_error(&self, mut response: HttpResponse<AsyncBody>) -> McpError {
        let status = response.status();
        let challenge = header(response.headers(), WWW_AUTHENTICATE_HEADER).map(str::to_owned);
        let mut body = String::new();
        let _ = response.body_mut().read_to_string(&mut body).await;
        if status != StatusCode::UNAUTHORIZED
            && let Some(Message::Response(Response {
                body: ResponseBody::Error(error),
                ..
            })) = serde_json::from_str(&body).ok().and_then(message::classify)
            && is_modern_error(error.code)
        {
            return McpError::rpc(&self.name, error);
        }
        McpError::HttpError {
            server: self.server(),
            status: status.as_u16(),
            reason: match challenge {
                Some(challenge) if status == StatusCode::UNAUTHORIZED => challenge,
                _ => body,
            },
        }
    }

    /// Reads a POST's response as it arrives. Requests and notifications on
    /// it belong to the call that opened it; a stream that ends before the
    /// call's response leaves the outcome unknown.
    async fn receive(
        self: &Arc<Self>,
        mut body: impl AsyncBufRead + Unpin,
        sse: bool,
        id: u64,
        route: &Route,
    ) -> Result<Value, McpError> {
        if !sse {
            let mut text = String::new();
            body.read_to_string(&mut text)
                .await
                .map_err(|e| self.invalid(e))?;
            let messages = match serde_json::from_str(&text).map_err(|e| self.invalid(e))? {
                Value::Array(items) => items,
                single => vec![single],
            };
            return messages
                .into_iter()
                .find_map(|message| self.settles(self.dispatch(message, Some(route))?, id))
                .unwrap_or_else(|| {
                    Err(self.invalid(format!("no response matching request id {id}")))
                });
        }

        let mut events = SseParser::default();
        let mut line = String::new();
        loop {
            line.clear();
            let ended = !matches!(body.read_line(&mut line).await, Ok(read) if read > 0);
            let event = if ended {
                events.finish()
            } else {
                events.line(&line)
            };
            if let Some(data) = event
                && let Ok(message) = serde_json::from_str(&data)
                && let Some(response) = self.dispatch(message, Some(route))
                && let Some(result) = self.settles(response, id)
            {
                return result;
            }
            if ended {
                return Err(McpError::ConnectionLost {
                    server: self.server(),
                });
            }
        }
    }

    /// A response without an id is an error the server could not attribute,
    /// which on a call's own stream can only be the call's.
    fn settles(&self, response: Response, id: u64) -> Option<Result<Value, McpError>> {
        response
            .id
            .as_ref()
            .is_none_or(|got| got.as_u64() == Some(id))
            .then(|| response.into_result(&self.name))
    }

    /// A response is handed back to whoever is reading for one. A request is
    /// served against `route`, or like stdio's when no call owns the stream.
    fn dispatch(self: &Arc<Self>, message: Value, route: Option<&Route>) -> Option<Response> {
        match message::classify(message)? {
            Message::Response(response) => Some(response),
            Message::Request(request) if self.inbound.era().modern().is_some() => {
                debug!(server = %self.name, method = %request.method, "ignoring a request from a modern MCP server, which asks through input_required");
                None
            }
            Message::Request(request) => {
                let route = route.cloned().or_else(|| self.inbound.routed());
                self.answer(request, route);
                None
            }
            Message::Notification(notification) => {
                self.inbound.notify(notification);
                None
            }
        }
    }

    fn answer(self: &Arc<Self>, request: ServerRequest, route: Option<Route>) {
        let shared = Arc::clone(self);
        smol::spawn(async move {
            if let Some(reply) = shared.inbound.answer(request, route).await
                && let Err(error) = shared.post(reply.to_string().into_bytes()).await
            {
                warn!(server = %shared.name, %error, "MCP reply to a server request failed");
            }
        })
        .detach();
    }

    fn post_detached(self: &Arc<Self>, message: Value) {
        let shared = Arc::clone(self);
        smol::spawn(async move {
            if let Err(error) = shared.post(message.to_string().into_bytes()).await {
                debug!(server = %shared.name, %error, "MCP message the server did not take");
            }
        })
        .detach();
    }

    /// Sends a message that has no response: a notification, or a reply to
    /// the server's own request, which the server acknowledges with 202.
    async fn post(&self, body: Vec<u8>) -> Result<(), McpError> {
        let negotiated = self.negotiated.lock().await.clone();
        let auth = self.auth.lock().await.clone();
        let request = self.build_request(Method::POST, body, &negotiated, auth.as_deref(), &[])?;
        let sent = async {
            self.client
                .send_async(request)
                .await
                .map(|response| response.status())
                .map_err(|e| self.write_failed(e))
        };
        let status = future::or(sent, async {
            Timer::after(self.timeout).await;
            Err(McpError::Timeout {
                server: self.server(),
                timeout_ms: self.timeout.as_millis() as u64,
            })
        })
        .await?;

        if !status.is_success() {
            return Err(McpError::HttpError {
                server: self.server(),
                status: status.as_u16(),
                reason: format!("{NOTIFICATION_REJECTED}: {status}"),
            });
        }
        Ok(())
    }

    /// Replaces a session the server forgot. Whoever finds the session still
    /// current makes the new one; anyone after reuses it.
    async fn reinitialize(self: &Arc<Self>, lost: &str) -> Result<(), McpError> {
        let _single = self.reinitializing.lock().await;
        {
            let mut negotiated = self.negotiated.lock().await;
            if negotiated.session_id.as_deref() != Some(lost) {
                return Ok(());
            }
            *negotiated = Negotiated::default();
        }
        info!(server = %self.name, "MCP session expired; initializing a new one");
        self.request(
            INITIALIZE_METHOD,
            Some(initialize_params()),
            &CallScope::none(),
        )
        .await?;
        self.post(self.encode(&JsonRpcNotification::new(INITIALIZED_NOTIFICATION, None))?)
            .await
    }

    /// Single-flight token refresh after a 401. Holds the `auth` lock across the
    /// refresh so concurrent callers park instead of racing the (rotating)
    /// refresh token. If the stored value no longer matches the one the failed
    /// request used, another caller already refreshed: reuse it.
    async fn refreshed_auth(&self, used: Option<&str>) -> Option<String> {
        let storage = self.storage.as_ref()?;
        let mut guard = self.auth.lock().await;

        if guard.as_deref() != used {
            return guard.clone();
        }

        match oauth::silent_refresh(storage, &self.name, &self.url, Some(&self.resolved)).await {
            Ok(Some(data)) => {
                let header = format!("Bearer {}", data.tokens?.access);
                *guard = Some(header.clone());

                info!(server = %self.name, "MCP OAuth token refreshed after 401");

                Some(header)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(server = %self.name, error = %e, "MCP OAuth token refresh failed");

                None
            }
        }
    }

    /// Keeps the GET stream open for as long as the transport lives,
    /// reconnecting after an end or a failure. A 405 means the server has
    /// none; repeated failures give up.
    async fn listen(self: Arc<Self>) {
        let mut events = SseParser::default();
        let mut failures = 0;
        loop {
            match self.open_stream(events.last_event_id.as_deref()).await {
                Ok(Some(body)) => match self.read_stream(body, &mut events).await {
                    true => failures = 0,
                    false => failures += 1,
                },
                Ok(None) => {
                    debug!(server = %self.name, "MCP server offers no GET stream");
                    return;
                }
                Err(error) => {
                    failures += 1;
                    debug!(server = %self.name, %error, failures, "MCP GET stream failed");
                }
            }
            if failures >= listen::MAX_LISTEN_FAILURES {
                warn!(server = %self.name, failures, "MCP GET stream gave up");
                return;
            }
            Timer::after(events.retry.unwrap_or_else(|| backoff(failures))).await;
        }
    }

    async fn open_stream(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<Option<AsyncBody>, McpError> {
        let negotiated = self.negotiated.lock().await.clone();
        let auth = self.auth.lock().await.clone();
        let mut request =
            self.build_request(Method::GET, Vec::new(), &negotiated, auth.as_deref(), &[])?;
        let headers = request.headers_mut();
        headers.remove(CONTENT_TYPE);
        headers.insert(ACCEPT, HeaderValue::from_static(CT_SSE));
        if let Some(id) = last_event_id.and_then(|id| HeaderValue::from_str(id).ok()) {
            headers.insert(LAST_EVENT_ID_HEADER, id);
        }
        let response = self
            .client
            .send_async(request)
            .await
            .map_err(|e| self.write_failed(e))?;
        match response.status() {
            StatusCode::METHOD_NOT_ALLOWED => Ok(None),
            status if status.is_success() => Ok(Some(response.into_body())),
            status => {
                if status == StatusCode::UNAUTHORIZED {
                    self.refreshed_auth(auth.as_deref()).await;
                }
                Err(McpError::HttpError {
                    server: self.server(),
                    status: status.as_u16(),
                    reason: LISTEN_REFUSED.into(),
                })
            }
        }
    }

    /// Whether the stream carried any message before it ended.
    async fn read_stream(self: &Arc<Self>, body: AsyncBody, events: &mut SseParser) -> bool {
        events.data.clear();
        let mut reader = BufReader::new(body);
        let mut line = String::new();
        let mut heard = false;
        loop {
            line.clear();
            if !matches!(reader.read_line(&mut line).await, Ok(read) if read > 0) {
                return heard;
            }
            if let Some(data) = events.line(&line)
                && let Ok(message) = serde_json::from_str(&data)
            {
                heard = true;
                self.dispatch(message, None);
            }
        }
    }
}

impl McpTransport for HttpTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        scope: &'a CallScope,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        self.shared.request(method, params, scope)
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let body = self
                .shared
                .encode(&JsonRpcNotification::new(method, params))?;
            self.shared.post(body).await
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.listener).take();
            let shared = &self.shared;
            let negotiated = shared.negotiated.lock().await.clone();
            if negotiated.session_id.is_none() {
                return;
            }

            let auth = shared.auth.lock().await.clone();
            let Ok(req) = shared.build_request(
                Method::DELETE,
                Vec::new(),
                &negotiated,
                auth.as_deref(),
                &[],
            ) else {
                return;
            };

            future::or(
                async {
                    let _ = shared.client.send_async(req).await;
                },
                async {
                    Timer::after(shared.timeout).await;
                },
            )
            .await;
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.shared.name
    }

    fn transport_kind(&self) -> &'static str {
        "http"
    }

    fn inbound(&self) -> Option<&Inbound> {
        Some(&self.shared.inbound)
    }

    fn listen(&self, capabilities: &ServerCapabilities) {
        let mut listener = lock(&self.listener);
        if listener.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        if shared.inbound.era().modern().is_none() {
            *listener = Some(smol::spawn(shared.listen()));
            return;
        }
        let server = Arc::clone(&shared.name);
        *listener = Some(smol::spawn(listen::subscribe(
            server,
            capabilities.listen_filter(),
            move |params| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let scope = CallScope::standing();
                    shared.request(LISTEN_METHOD, Some(params), &scope).await
                })
            },
        )));
    }
}

fn header(headers: &HeaderMap, name: impl AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `raw` as it goes in a header: unchanged when plain visible ASCII carries
/// it exactly, else base64 between the sentinel markers. Leading or trailing
/// whitespace, and anything that already looks like the sentinel, is
/// encoded too.
fn header_value(raw: &str) -> Option<HeaderValue> {
    let blank = |c: char| c == ' ' || c == '\t';
    let plain = raw
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'\t')
        && !raw.starts_with(blank)
        && !raw.ends_with(blank)
        && !(raw.starts_with(SENTINEL_PREFIX) && raw.ends_with(SENTINEL_SUFFIX));
    let text = match plain {
        true => raw.to_owned(),
        false => format!("{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}", BASE64.encode(raw)),
    };
    HeaderValue::try_from(text).ok()
}

/// The arguments a tool's `inputSchema` marks for mirroring, or why the
/// marks disqualify the tool: each must name a header token no other mark
/// names, sit on an integer, string or boolean property, and be reached
/// through `properties` alone.
fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut headers = Vec::new();
    collect_param_headers(schema, &mut Vec::new(), &mut headers)?;
    if marks(schema) != headers.len() {
        return Err(UNREACHABLE_HEADER.into());
    }
    let mut names = HashSet::new();
    if let Some(duplicate) = headers.iter().find(|header| !names.insert(&header.name)) {
        return Err(format!("{DUPLICATE_HEADER} {}", duplicate.name));
    }
    Ok(headers)
}

fn collect_param_headers(
    schema: &Value,
    path: &mut Vec<String>,
    headers: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    let Some(properties) = schema.get(PROPERTIES_KEY).and_then(Value::as_object) else {
        return Ok(());
    };
    for (key, property) in properties {
        path.push(key.clone());
        if let Some(mark) = property.get(X_MCP_HEADER) {
            headers.push(param_header(mark, property, path)?);
        }
        collect_param_headers(property, path, headers)?;
        path.pop();
    }
    Ok(())
}

fn param_header(mark: &Value, property: &Value, path: &[String]) -> Result<ParamHeader, String> {
    let name = mark
        .as_str()
        .filter(|name| !name.is_empty() && name.bytes().all(is_token_byte))
        .ok_or_else(|| format!("{INVALID_HEADER_NAME} {mark}"))?;
    if !header_type(&property[TYPE_KEY]) {
        return Err(format!("{NOT_PRIMITIVE} {}", path.join(".")));
    }
    let name = HeaderName::try_from(format!("{PARAM_HEADER_PREFIX}{name}"))
        .map_err(|error| error.to_string())?;
    Ok(ParamHeader {
        name,
        path: path.to_vec(),
    })
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || TOKEN_SYMBOLS.contains(&byte)
}

/// An integer, string or boolean, optionally nullable.
fn header_type(kind: &Value) -> bool {
    let mut kinds = match kind {
        Value::Array(kinds) => kinds.iter().filter(|kind| *kind != NULL_TYPE).collect(),
        kind => vec![kind],
    }
    .into_iter();
    matches!(
        (kinds.next().and_then(Value::as_str), kinds.next()),
        (Some(kind), None) if HEADER_TYPES.contains(&kind)
    )
}

/// Every `x-mcp-header` in `schema`, wherever it sits. The keys of a
/// `properties` map name properties, so one called `x-mcp-header` is no mark.
fn marks(schema: &Value) -> usize {
    match schema {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| match (key.as_str(), value) {
                (X_MCP_HEADER, _) => 1,
                (PROPERTIES_KEY, Value::Object(properties)) => properties.values().map(marks).sum(),
                _ => marks(value),
            })
            .sum(),
        Value::Array(items) => items.iter().map(marks).sum(),
        _ => 0,
    }
}

/// Server-sent events, one line at a time, so a message is handled the
/// moment its event ends rather than when the stream does. The event id and
/// `retry:` outlive one stream, since a reconnect resumes from them.
#[derive(Default)]
struct SseParser {
    data: String,
    last_event_id: Option<String>,
    retry: Option<Duration>,
}

impl SseParser {
    /// The data of the event `line` ends, if it ends one.
    fn line(&mut self, line: &str) -> Option<String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return self.finish();
        }
        if line.starts_with(SSE_COMMENT) {
            return None;
        }
        let (field, value) = line
            .split_once(SSE_COMMENT)
            .map_or((line, ""), |(field, value)| {
                (field, value.strip_prefix(' ').unwrap_or(value))
            });
        match field {
            SSE_DATA => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            SSE_ID => self.last_event_id = Some(value.to_owned()),
            SSE_RETRY => {
                if let Ok(millis) = value.parse() {
                    self.retry = Some(Duration::from_millis(millis));
                }
            }
            _ => {}
        }
        None
    }

    /// The pending event, for a stream that ended without the blank line.
    fn finish(&mut self) -> Option<String> {
        let mut data = std::mem::take(&mut self.data);
        data.pop()?;
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    use caudra_storage::auth::{McpAuthData, OAuthTokens, save_mcp_auth};
    use std::io::{BufRead, BufReader as StdBufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;

    use crate::mcp::McpToolMeta;
    use crate::mcp::era::{self, ProtocolEra};
    use crate::mcp::inbound::{ListKind, NOTIFICATIONS_KEY};
    use crate::mcp::protocol::{
        DISCOVER_METHOD, LATEST_PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION, ListCapability,
        META_KEY, PROTOCOL_VERSION_META, SUBSCRIPTION_ID_META, TOOLS_LIST_CHANGED_FILTER,
    };
    use crate::mcp::transport::{call_tool, list_tools};

    const UNREACHABLE: &str = "http://127.0.0.1:1/mcp";
    const NOTIFICATION: &str =
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
    const REQUEST_ID: u64 = 7;
    const RESPONSE_EVENT: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
    const STALE_RESPONSE_EVENT: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stale\":true}}\n\n";
    const NULL_ID_ERROR_EVENT: &str = "data: {\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"parse error\"}}\n\n";
    const SSE_NO_RESULT_NO_ERROR: &str = "data: {\"jsonrpc\":\"2.0\",\"id\":7}\n\n";
    const SSE_SAME_ID_REQUEST: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"vendor/x\"}\n\n";
    const JSON_NULL_ID_ERROR: &str =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#;
    const JSON_ERROR_RESPONSE: &str =
        r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"method not found"}}"#;
    const JSON_FOREIGN_ERROR: &str =
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#;
    const JSON_NOTIFICATION: &str =
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#;
    const JSON_PING_REQUEST: &str = r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#;
    const JSON_NO_RESULT_NO_ERROR: &str = r#"{"jsonrpc":"2.0","id":7}"#;
    const JSON_NULL_ERROR_WITH_RESULT: &str =
        r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true},"error":null}"#;
    const JSON_NULL_ERROR_NO_RESULT: &str = r#"{"jsonrpc":"2.0","id":7,"error":null}"#;
    const NEGOTIATED_VERSION: &str = "2025-03-26";
    const OLD_BEARER: &str = "Bearer old-token";
    const NEW_BEARER: &str = "Bearer new-token";
    const SESSION: &str = "s1";
    const RENEWED_SESSION: &str = "s2";
    const EVENT_ID: &str = "42";
    const RETRY_MS: u64 = 10;
    const LIST_CHANGED: &str = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
    const GET: &str = "GET";
    /// How long the fake server waits for the client to do its part, so a
    /// broken client fails the test instead of hanging it.
    const CLIENT_WAIT: Duration = Duration::from_secs(5);
    /// What a long-lived stream sends while idle. A client learns its peer
    /// dropped the stream only when the next bytes arrive.
    const KEEPALIVE: &[u8] = b": keepalive\n\n";
    const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(20);

    fn rpc_ok(id: u64) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":true}}}}"#)
    }

    struct Req {
        method: String,
        path: String,
        auth: Option<String>,
        protocol: Option<String>,
        session: Option<String>,
        last_event_id: Option<String>,
        headers: HashMap<String, String>,
        body: Value,
    }

    impl Req {
        fn id(&self) -> u64 {
            self.body["id"].as_u64().unwrap_or_default()
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).map(String::as_str)
        }
    }

    fn read_request(stream: &TcpStream) -> Option<Req> {
        let mut reader = StdBufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let mut req = Req {
            method: parts.next()?.to_owned(),
            path: parts.next()?.to_owned(),
            auth: None,
            protocol: None,
            session: None,
            last_event_id: None,
            headers: HashMap::new(),
            body: Value::Null,
        };
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else {
                continue;
            };
            let value = value.trim().to_owned();
            let name = name.to_ascii_lowercase();
            match name.as_str() {
                "authorization" => req.auth = Some(value.clone()),
                "content-length" => content_length = value.parse().unwrap_or(0),
                PROTOCOL_HEADER => req.protocol = Some(value.clone()),
                SESSION_HEADER => req.session = Some(value.clone()),
                LAST_EVENT_ID_HEADER => req.last_event_id = Some(value.clone()),
                _ => {}
            }
            req.headers.insert(name, value);
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).ok()?;
        req.body = serde_json::from_slice(&body).unwrap_or_default();
        Some(req)
    }

    fn respond(mut stream: TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
        let content_type = if body.starts_with(SSE_DATA) || body.starts_with(SSE_ID) {
            CT_SSE
        } else {
            CT_JSON
        };
        let extra: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len(),
        );
        let _ = stream.write_all(response.as_bytes());
    }

    /// Opens an SSE response that stays open until the handler returns.
    fn open_events(stream: &mut TcpStream) {
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Type: {CT_SSE}\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
    }

    fn send_event(stream: &mut TcpStream, message: &Value) {
        let _ = stream.write_all(format!("{SSE_DATA}: {message}\n\n").as_bytes());
    }

    /// One thread per connection, so a response held open does not stall
    /// the request that completes it.
    fn spawn_raw<F>(make_handler: impl FnOnce(String) -> F) -> String
    where
        F: Fn(Req, TcpStream) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(make_handler(base.clone()));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    if let Some(req) = read_request(&stream) {
                        handler(req, stream);
                    }
                });
            }
        });
        base
    }

    fn spawn_server<F>(make_handler: impl FnOnce(String) -> F) -> String
    where
        F: Fn(&Req) -> (u16, String) + Send + Sync + 'static,
    {
        spawn_raw(|base| {
            let handler = make_handler(base);
            move |req: Req, stream: TcpStream| {
                let (status, body) = handler(&req);
                respond(stream, status, &[], &body);
            }
        })
    }

    fn stored_auth(server_url: &str, access: &str, refresh: &str) -> McpAuthData {
        McpAuthData {
            server_url: server_url.to_string(),
            tokens: Some(OAuthTokens {
                access: access.to_string(),
                refresh: refresh.to_string(),
                expires: 0,
                account_id: None,
            }),
            client_id: "cid".to_string(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: None,
            token_endpoint: None,
            issuer: None,
        }
    }

    fn transport_with(
        url: &str,
        headers: HashMap<String, String>,
        storage: Option<StateDir>,
    ) -> HttpTransport {
        HttpTransport::new("srv", url, &headers, &[], CLIENT_WAIT, storage).unwrap()
    }

    fn send(transport: &HttpTransport, method: &str) -> Result<Value, McpError> {
        smol::block_on(transport.send_request(method, None, &CallScope::none()))
    }

    fn oauth_routes(base: &str, req: &Req) -> Option<(u16, String)> {
        if req.path.contains("oauth-protected-resource") {
            return Some((
                200,
                format!(r#"{{"authorization_servers":["{base}"],"resource":"{base}/mcp"}}"#),
            ));
        }

        if req.path.contains("oauth-authorization-server")
            || req.path.contains("openid-configuration")
        {
            return Some((
                200,
                format!(
                    r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ));
        }

        if req.path == "/token" {
            return Some((
                200,
                r#"{"access_token":"new-token","expires_in":3600}"#.into(),
            ));
        }

        None
    }

    fn sse_messages(input: &str) -> Vec<Value> {
        let mut parser = SseParser::default();
        let mut events: Vec<String> = input
            .split_inclusive('\n')
            .filter_map(|line| parser.line(line))
            .collect();
        events.extend(parser.finish());
        events
            .iter()
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    #[test_case("data: {\"id\":1}\n\n",                                      &[json!({"id":1})]                  ; "single_event")]
    #[test_case("data: {\"id\":1}\n\ndata: {\"id\":2}\n\n",                  &[json!({"id":1}), json!({"id":2})] ; "multiple_events")]
    #[test_case("data: {\"id\":1,\ndata:  \"result\":{}}\n\n",               &[json!({"id":1, "result":{}})]     ; "multiline_data")]
    #[test_case(": comment\ndata: {\"id\":1}\n\n",                           &[json!({"id":1})]                  ; "ignores_comments")]
    #[test_case("event: message\nid: 42\nretry: 5000\ndata: {\"id\":1}\n\n", &[json!({"id":1})]                  ; "ignores_non_data_fields")]
    #[test_case("",                                                          &[]                                 ; "empty_body")]
    #[test_case("event: ping\n\n",                                           &[]                                 ; "no_data_field")]
    #[test_case("data: not json\n\ndata: {\"id\":1}\n\n",                    &[json!({"id":1})]                  ; "malformed_json_skipped")]
    #[test_case("data: {\"id\":1}",                                          &[json!({"id":1})]                  ; "no_trailing_newline")]
    #[test_case("data:{\"id\":1}\n\n",                                       &[json!({"id":1})]                  ; "no_space_after_colon")]
    #[test_case("data: {\"id\":1}\r\n\r\n",                                  &[json!({"id":1})]                  ; "crlf_lines")]
    fn parse_sse(input: &str, expected: &[Value]) {
        assert_eq!(sse_messages(input), expected);
    }

    #[test]
    fn sse_keeps_the_event_id_and_retry_for_a_reconnect() {
        let mut parser = SseParser::default();
        for line in [
            "id: 42\n",
            "retry: 5000\n",
            "retry: soon\n",
            "data: {}\n",
            "\n",
        ] {
            parser.line(line);
        }
        assert_eq!(parser.last_event_id.as_deref(), Some(EVENT_ID));
        assert_eq!(parser.retry, Some(Duration::from_millis(5000)));
    }

    enum Expected {
        Ok(Value),
        Invalid,
        Rpc(i64),
        Lost,
    }

    fn receive(body: &str, sse: bool) -> Result<Value, McpError> {
        let transport = transport_with(UNREACHABLE, HashMap::new(), None);
        let scope = CallScope::none();
        let tracked = transport.shared.inbound.track(REQUEST_ID, &scope);
        smol::block_on(
            transport
                .shared
                .receive(body.as_bytes(), sse, REQUEST_ID, tracked.route()),
        )
    }

    fn assert_received(result: Result<Value, McpError>, expected: Expected) {
        match expected {
            Expected::Ok(value) => assert_eq!(result.unwrap(), value),
            Expected::Invalid => assert!(
                matches!(result, Err(McpError::InvalidResponse { .. })),
                "{result:?}"
            ),
            Expected::Rpc(code) => assert!(
                matches!(result, Err(McpError::RpcError { code: got, .. }) if got == code),
                "{result:?}"
            ),
            Expected::Lost => assert!(
                matches!(result, Err(McpError::ConnectionLost { .. })),
                "{result:?}"
            ),
        }
    }

    #[test_case(&format!("{NOTIFICATION}{RESPONSE_EVENT}"),         Expected::Ok(json!({"ok": true})) ; "skips_interleaved_notifications")]
    #[test_case(&format!("{STALE_RESPONSE_EVENT}{RESPONSE_EVENT}"), Expected::Ok(json!({"ok": true})) ; "skips_stale_response_ids")]
    #[test_case(&format!("{SSE_SAME_ID_REQUEST}{RESPONSE_EVENT}"),  Expected::Ok(json!({"ok": true})) ; "a_request_reusing_the_id_is_no_response")]
    #[test_case(NULL_ID_ERROR_EVENT,                                Expected::Rpc(-32700)             ; "unattributed_error_is_the_calls")]
    #[test_case(NOTIFICATION,                                       Expected::Lost                    ; "stream_ending_early_is_lost")]
    #[test_case(SSE_NO_RESULT_NO_ERROR,                             Expected::Invalid                 ; "no_result_no_error_rejected")]
    fn sse_response_matching(body: &str, expected: Expected) {
        assert_received(receive(body, true), expected);
    }

    #[test_case(&rpc_ok(7),                                          Expected::Ok(json!({"ok": true})) ; "matching_id_result")]
    #[test_case(&rpc_ok(3),                                          Expected::Invalid                 ; "wrong_id_rejected")]
    #[test_case(JSON_NULL_ID_ERROR,                                  Expected::Rpc(-32700)             ; "null_id_error_accepted")]
    #[test_case(JSON_ERROR_RESPONSE,                                 Expected::Rpc(-32601)             ; "matching_id_error")]
    #[test_case(JSON_FOREIGN_ERROR,                                  Expected::Invalid                 ; "foreign_id_error_rejected")]
    #[test_case(JSON_NOTIFICATION,                                   Expected::Invalid                 ; "notification_rejected")]
    #[test_case("not json",                                          Expected::Invalid                 ; "malformed_rejected")]
    #[test_case(JSON_NO_RESULT_NO_ERROR,                             Expected::Invalid                 ; "matching_id_no_result_no_error")]
    #[test_case(JSON_NULL_ERROR_WITH_RESULT,                         Expected::Ok(json!({"ok": true})) ; "null_error_next_to_result_accepted")]
    #[test_case(JSON_NULL_ERROR_NO_RESULT,                           Expected::Invalid                 ; "null_error_without_result_rejected")]
    #[test_case(&format!("[{JSON_NOTIFICATION},{}]", rpc_ok(7)),     Expected::Ok(json!({"ok": true})) ; "batch_skips_leading_notification")]
    #[test_case(&format!("[{JSON_PING_REQUEST},{}]", rpc_ok(7)),     Expected::Ok(json!({"ok": true})) ; "batch_skips_ping_request")]
    #[test_case(&format!("[{},{}]", rpc_ok(3), rpc_ok(7)),           Expected::Ok(json!({"ok": true})) ; "batch_skips_stale_ids")]
    #[test_case(&format!("[{JSON_NOTIFICATION}]"),                   Expected::Invalid                 ; "batch_notification_only_rejected")]
    #[test_case(&format!("[{JSON_FOREIGN_ERROR},{}]", rpc_ok(7)),    Expected::Ok(json!({"ok": true})) ; "batch_foreign_error_skipped")]
    #[test_case(&format!("[{JSON_NULL_ID_ERROR},{}]", rpc_ok(7)),    Expected::Rpc(-32700)             ; "batch_null_id_error_wins")]
    #[test_case("[]",                                                Expected::Invalid                 ; "batch_empty_rejected")]
    fn json_response_matching(body: &str, expected: Expected) {
        assert_received(receive(body, false), expected);
    }

    #[test]
    fn build_request_applies_all_headers() {
        let headers = HashMap::from([("x-custom".to_string(), "yes".to_string())]);
        let transport = transport_with(UNREACHABLE, headers, None);
        let negotiated = Negotiated {
            session_id: Some(SESSION.to_string()),
            protocol_version: Some(NEGOTIATED_VERSION.to_string()),
        };

        let req = transport
            .shared
            .build_request(Method::POST, Vec::new(), &negotiated, Some(OLD_BEARER), &[])
            .unwrap();

        let headers = req.headers();
        assert_eq!(headers.get(SESSION_HEADER).unwrap(), SESSION);
        assert_eq!(headers.get(PROTOCOL_HEADER).unwrap(), NEGOTIATED_VERSION);
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), OLD_BEARER);
        assert_eq!(headers.get("x-custom").unwrap(), "yes");
    }

    #[test]
    fn server_negotiated_protocol_version_echoed_after_initialize() {
        let base = spawn_server(|_| {
            move |req: &Req| match req.protocol.as_deref() {
                None => (
                    200,
                    format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{NEGOTIATED_VERSION}"}}}}"#
                    ),
                ),
                Some(NEGOTIATED_VERSION) => (200, rpc_ok(2)),
                Some(_) => (400, String::new()),
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        send(&transport, INITIALIZE_METHOD).unwrap();

        let result = send(&transport, TOOLS_LIST_METHOD).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn refreshes_token_and_retries_on_401() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());

        let base = spawn_server(|base| {
            move |req: &Req| {
                if let Some(resp) = oauth_routes(&base, req) {
                    return resp;
                }
                if req.auth.as_deref() == Some(NEW_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });

        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, "srv", &stored_auth(&url, "old-token", "r1")).unwrap();

        let transport = transport_with(&url, HashMap::new(), Some(storage.clone()));
        let result = send(&transport, TOOLS_LIST_METHOD).unwrap();
        assert_eq!(result, json!({"ok": true}));

        let saved = load_mcp_auth(&storage, "srv", &url).unwrap();
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access, "new-token");
        assert_eq!(tokens.refresh, "r1");
    }

    /// Home Assistant's `/api/mcp` answers 401 to a user who is not an
    /// administrator, whatever the token. A fresh token that is still turned
    /// away is a refusal, not a reason to sign in again.
    #[test]
    fn a_fresh_token_still_turned_away_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let base = spawn_server(|base| {
            move |req: &Req| oauth_routes(&base, req).unwrap_or((401, String::new()))
        });
        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, "srv", &stored_auth(&url, "old-token", "r1")).unwrap();

        let transport = transport_with(&url, HashMap::new(), Some(storage));
        let err = send(&transport, TOOLS_LIST_METHOD).unwrap_err();

        assert!(matches!(err, McpError::Refused { .. }), "{err:?}");
    }

    #[test]
    fn unauthorized_without_storage_fails_without_retry() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        let base = spawn_server(move |_| {
            move |_req: &Req| {
                hits_srv.fetch_add(1, Ordering::SeqCst);
                (401, String::new())
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let err = send(&transport, TOOLS_LIST_METHOD).unwrap_err();
        assert!(matches!(err, McpError::HttpError { status: 401, .. }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn config_authorization_header_is_sent() {
        let base = spawn_server(|_| {
            move |req: &Req| {
                if req.auth.as_deref() == Some(OLD_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });

        let headers = HashMap::from([("Authorization".to_string(), OLD_BEARER.to_string())]);
        let transport = transport_with(&format!("{base}/mcp"), headers, None);
        let result = send(&transport, TOOLS_LIST_METHOD).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn stored_token_injected_at_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());

        let base = spawn_server(|_| {
            move |req: &Req| {
                if req.auth.as_deref() == Some(OLD_BEARER) {
                    (200, rpc_ok(1))
                } else {
                    (401, String::new())
                }
            }
        });
        let url = format!("{base}/mcp");
        save_mcp_auth(&storage, "srv", &stored_auth(&url, "old-token", "r1")).unwrap();

        let transport = transport_with(&url, HashMap::new(), Some(storage));
        let result = send(&transport, TOOLS_LIST_METHOD).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    /// The call's stream carries a `ping` that reuses the call's own id, and
    /// holds the call's response back until the ping's reply was POSTed.
    #[test]
    fn a_server_request_on_a_call_stream_is_answered_by_post() {
        let (replies_tx, replies_rx) = mpsc::channel::<Value>();
        let replies_rx = SyncMutex::new(replies_rx);
        let base = spawn_raw(move |_| {
            move |req: Req, mut stream: TcpStream| {
                if req.body.get("method").is_none() {
                    let _ = replies_tx.send(req.body);
                    return respond(stream, 202, &[], "");
                }
                open_events(&mut stream);
                send_event(
                    &mut stream,
                    &json!({"jsonrpc": "2.0", "id": req.id(), "method": "ping"}),
                );
                let reply = lock(&replies_rx)
                    .recv_timeout(CLIENT_WAIT)
                    .unwrap_or_default();
                send_event(
                    &mut stream,
                    &json!({"jsonrpc": "2.0", "id": req.id(), "result": {"reply": reply}}),
                );
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let result = send(&transport, TOOLS_CALL_METHOD).unwrap();
        assert_eq!(
            result["reply"],
            json!({"jsonrpc": "2.0", "id": 1, "result": {}})
        );
    }

    #[test]
    fn a_lost_session_is_initialized_again() {
        let initializations = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&initializations);
        let base = spawn_raw(move |_| {
            move |req: Req, stream: TcpStream| match req.body["method"].as_str() {
                Some(INITIALIZE_METHOD) => {
                    let session = match counted.fetch_add(1, Ordering::SeqCst) {
                        0 => SESSION,
                        _ => RENEWED_SESSION,
                    };
                    respond(stream, 200, &[(SESSION_HEADER, session)], &rpc_ok(req.id()));
                }
                Some(INITIALIZED_NOTIFICATION) => respond(stream, 202, &[], ""),
                _ if req.session.as_deref() == Some(RENEWED_SESSION) => {
                    respond(stream, 200, &[], &rpc_ok(req.id()));
                }
                _ => respond(stream, 404, &[], ""),
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        send(&transport, INITIALIZE_METHOD).unwrap();
        let result = send(&transport, TOOLS_LIST_METHOD).unwrap();

        assert_eq!(result, json!({"ok": true}));
        assert_eq!(initializations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_404_without_a_session_is_an_error() {
        let base = spawn_server(|_| move |_req: &Req| (404, String::new()));
        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let err = send(&transport, TOOLS_LIST_METHOD).unwrap_err();
        assert!(matches!(err, McpError::HttpError { status: 404, .. }));
    }

    #[test]
    fn the_get_stream_delivers_list_changes_and_resumes_from_the_last_event() {
        let (resumed_tx, resumed_rx) = mpsc::channel();
        let base = spawn_raw(move |_| {
            move |req: Req, stream: TcpStream| {
                if req.method != GET {
                    return respond(stream, 202, &[], "");
                }
                match req.last_event_id {
                    None => respond(
                        stream,
                        200,
                        &[],
                        &format!(
                            "{SSE_ID}: {EVENT_ID}\n{SSE_RETRY}: {RETRY_MS}\n{SSE_DATA}: {LIST_CHANGED}\n\n"
                        ),
                    ),
                    Some(id) => {
                        let _ = resumed_tx.send(id);
                        respond(stream, 405, &[], "");
                    }
                }
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let changes = transport.shared.inbound.list_changes();
        transport.listen(&ServerCapabilities::default());

        assert_eq!(changes.recv_timeout(CLIENT_WAIT), Ok(ListKind::Tools));
        assert_eq!(
            resumed_rx.recv_timeout(CLIENT_WAIT).as_deref(),
            Ok(EVENT_ID)
        );
    }

    const MODERN_DISCOVER: &str = r#""result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"home-assistant","version":"2026.10"}}}"#;
    const LEGACY_INITIALIZE: &str = r#""result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"executor","version":"1.29"}}"#;
    const UNSUPPORTED_WITH_OURS: &str = r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2026-07-28"],"requested":"2026-07-28"}}"#;
    const UNSUPPORTED_WITHOUT_OURS: &str = r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2025-11-25"],"requested":"2026-07-28"}}"#;
    const METHOD_NOT_FOUND_REPLY: &str = r#""error":{"code":-32601,"message":"Method not found"}"#;
    const HEADER_MISMATCH_REPLY: &str = r#""error":{"code":-32020,"message":"Header mismatch"}"#;
    const DONE_REPLY: &str = r#""result":{"content":[{"type":"text","text":"done"}]}"#;
    const DONE: &str = "done";
    const MARKED_TOOL: &str = "query";
    const BROKEN_TOOL: &str = "broken";
    const REGION: &str = "us-west1";
    const REGION_HEADER: &str = "mcp-param-region";
    const NON_ASCII: &str = "Hello, 世界";
    const NON_ASCII_ENCODED: &str = "=?base64?SGVsbG8sIOS4lueVjA==?=";

    fn rpc_reply(id: u64, reply: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},{reply}}}"#)
    }

    fn modern(transport: &HttpTransport) {
        transport.shared.inbound.set_era(ProtocolEra::Modern {
            version: LATEST_PROTOCOL_VERSION,
        });
    }

    enum Era {
        Modern,
        Legacy,
        Refused,
    }

    #[test_case(200, MODERN_DISCOVER, Era::Modern, 1 ; "discover_result")]
    #[test_case(400, "", Era::Legacy, 1 ; "bad_request_with_an_empty_body")]
    #[test_case(400, UNSUPPORTED_WITH_OURS, Era::Modern, 2 ; "bad_request_with_a_modern_error")]
    #[test_case(400, UNSUPPORTED_WITHOUT_OURS, Era::Legacy, 1 ; "bad_request_without_our_version")]
    #[test_case(404, METHOD_NOT_FOUND_REPLY, Era::Legacy, 1 ; "not_found_with_method_not_found")]
    #[test_case(200, METHOD_NOT_FOUND_REPLY, Era::Legacy, 1 ; "ok_with_method_not_found")]
    #[test_case(401, "", Era::Refused, 1 ; "unauthorized")]
    fn http_selects_the_era_from_the_probe(
        status: u16,
        reply: &'static str,
        era: Era,
        discovers: usize,
    ) {
        let (probes_tx, probes) = mpsc::channel::<Req>();
        let answered = AtomicUsize::new(0);
        let base = spawn_raw(move |_| {
            move |req: Req, stream: TcpStream| match req.body["method"].as_str() {
                Some(DISCOVER_METHOD) => {
                    let (status, reply) = match answered.fetch_add(1, Ordering::SeqCst) {
                        0 => (status, reply),
                        _ => (200, MODERN_DISCOVER),
                    };
                    let body = match reply {
                        "" => String::new(),
                        reply => rpc_reply(req.id(), reply),
                    };
                    let _ = probes_tx.send(req);
                    respond(stream, status, &[], &body);
                }
                Some(INITIALIZE_METHOD) => respond(
                    stream,
                    200,
                    &[(SESSION_HEADER, SESSION)],
                    &rpc_reply(req.id(), LEGACY_INITIALIZE),
                ),
                _ => respond(stream, 202, &[], ""),
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        let connected = smol::block_on(era::connect(&transport, CLIENT_WAIT));
        let probes: Vec<Req> = probes.try_iter().collect();
        assert_eq!(probes.len(), discovers);
        for probe in &probes {
            assert_eq!(probe.header(PROTOCOL_HEADER), Some(LATEST_PROTOCOL_VERSION));
            assert_eq!(probe.header(METHOD_HEADER), Some(DISCOVER_METHOD));
            assert_eq!(probe.session, None);
            assert_eq!(
                probe.body["params"][META_KEY][PROTOCOL_VERSION_META],
                LATEST_PROTOCOL_VERSION
            );
        }
        match era {
            Era::Modern => {
                assert_eq!(connected.unwrap().protocol_version, LATEST_PROTOCOL_VERSION);
                assert_eq!(transport.era().modern(), Some(LATEST_PROTOCOL_VERSION));
            }
            Era::Legacy => {
                assert_eq!(connected.unwrap().protocol_version, LEGACY_PROTOCOL_VERSION);
                assert_eq!(transport.era().modern(), None);
            }
            Era::Refused => assert!(
                matches!(connected, Err(McpError::HttpError { status: 401, .. })),
                "{connected:?}"
            ),
        }
    }

    #[test_case(TOOLS_CALL_METHOD, json!({"name": "get_weather"}), Some("get_weather") ; "tools_call")]
    #[test_case(PROMPTS_GET_METHOD, json!({"name": "café"}), Some("=?base64?Y2Fmw6k=?=") ; "prompts_get_encoded")]
    #[test_case(RESOURCES_READ_METHOD, json!({"uri": "file:///projects/a b.json"}), Some("file:///projects/a b.json") ; "resources_read")]
    #[test_case(TOOLS_LIST_METHOD, json!({"name": "get_weather"}), None ; "tools_list")]
    #[test_case(LISTEN_METHOD, Value::Null, None ; "listen")]
    fn modern_headers_mirror_the_body(method: &str, params: Value, name: Option<&str>) {
        let transport = transport_with(UNREACHABLE, HashMap::new(), None);
        let headers: HeaderMap = transport
            .shared
            .mirrored(LATEST_PROTOCOL_VERSION, method, Some(&params))
            .into_iter()
            .collect();
        assert_eq!(
            header(&headers, PROTOCOL_HEADER),
            Some(LATEST_PROTOCOL_VERSION)
        );
        assert_eq!(header(&headers, METHOD_HEADER), Some(method));
        assert_eq!(header(&headers, NAME_HEADER), name);
    }

    #[test_case("us-west1", "us-west1" ; "plain")]
    #[test_case(NON_ASCII, NON_ASCII_ENCODED ; "non_ascii")]
    #[test_case(" padded ", "=?base64?IHBhZGRlZCA=?=" ; "surrounding_spaces")]
    #[test_case("\tlead", "=?base64?CWxlYWQ=?=" ; "leading_tab")]
    #[test_case("line1\nline2", "=?base64?bGluZTEKbGluZTI=?=" ; "newline")]
    #[test_case("=?base64?literal?=", "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=" ; "looks_like_the_sentinel")]
    #[test_case("a\tb c", "a\tb c" ; "inner_tab_and_space")]
    #[test_case("", "" ; "empty")]
    fn header_values_take_the_sentinel_when_plain_ascii_cannot_carry_them(
        raw: &str,
        expected: &str,
    ) {
        assert_eq!(header_value(raw).unwrap(), expected);
    }

    #[test_case(json!({"properties": {"region": {"type": "string", "x-mcp-header": "Region"}}}), Some(1) ; "string")]
    #[test_case(json!({"properties": {"target": {"type": "object", "properties": {"zone": {"type": "integer", "x-mcp-header": "Zone"}, "dry": {"type": "boolean", "x-mcp-header": "Dry-Run"}}}}}), Some(2) ; "nested_through_properties")]
    #[test_case(json!({"properties": {"region": {"type": ["string", "null"], "x-mcp-header": "Region"}}}), Some(1) ; "nullable")]
    #[test_case(json!({"properties": {"x-mcp-header": {"type": "string"}}}), Some(0) ; "a_property_named_like_the_mark")]
    #[test_case(json!({"properties": {"region": {"type": "string", "x-mcp-header": ""}}}), None ; "empty_name")]
    #[test_case(json!({"properties": {"region": {"type": "string", "x-mcp-header": "Re gion"}}}), None ; "not_a_token")]
    #[test_case(json!({"properties": {"region": {"type": "string", "x-mcp-header": "Region\r\nX: 1"}}}), None ; "control_characters")]
    #[test_case(json!({"properties": {"region": {"type": "string", "x-mcp-header": 7}}}), None ; "not_a_string")]
    #[test_case(json!({"properties": {"ratio": {"type": "number", "x-mcp-header": "Ratio"}}}), None ; "number")]
    #[test_case(json!({"properties": {"tags": {"type": "object", "x-mcp-header": "Tags"}}}), None ; "object")]
    #[test_case(json!({"properties": {"either": {"type": ["string", "integer"], "x-mcp-header": "Either"}}}), None ; "two_types")]
    #[test_case(json!({"properties": {"a": {"type": "string", "x-mcp-header": "Region"}, "b": {"type": "string", "x-mcp-header": "region"}}}), None ; "duplicate_ignoring_case")]
    #[test_case(json!({"properties": {"list": {"type": "array", "items": {"type": "string", "x-mcp-header": "Item"}}}}), None ; "under_items")]
    #[test_case(json!({"properties": {"choice": {"anyOf": [{"type": "string", "x-mcp-header": "Choice"}]}}}), None ; "under_any_of")]
    #[test_case(json!({"$defs": {"r": {"type": "string", "x-mcp-header": "Region"}}, "properties": {"region": {"$ref": "#/$defs/r"}}}), None ; "behind_a_ref")]
    fn x_mcp_header_marks_are_checked(schema: Value, marks: Option<usize>) {
        assert_eq!(
            param_headers(&schema).map(|headers| headers.len()).ok(),
            marks
        );
    }

    #[test_case(json!({"target": {"region": REGION}}), Some(REGION) ; "string")]
    #[test_case(json!({"target": {"region": NON_ASCII}}), Some(NON_ASCII_ENCODED) ; "encoded_string")]
    #[test_case(json!({"target": {"region": -7}}), Some("-7") ; "integer")]
    #[test_case(json!({"target": {"region": false}}), Some("false") ; "boolean")]
    #[test_case(json!({"target": {"region": null}}), None ; "null")]
    #[test_case(json!({"target": {}}), None ; "absent")]
    fn marked_arguments_are_mirrored_at_their_path(arguments: Value, expected: Option<&str>) {
        let param = ParamHeader {
            name: HeaderName::from_static(REGION_HEADER),
            path: vec!["target".into(), "region".into()],
        };
        let mirrored = param.mirror(&arguments);
        assert_eq!(
            mirrored.as_ref().map(|(_, value)| value.to_str().unwrap()),
            expected
        );
        assert!(mirrored.is_none_or(|(name, _)| name == param.name));
    }

    fn tools_page(id: u64, marked: bool) -> String {
        let mut region = json!({"type": "string"});
        if marked {
            region[X_MCP_HEADER] = json!("Region");
        }
        let broken = json!({"type": "object", "properties": {"ratio": {"type": "number", X_MCP_HEADER: "Ratio"}}});
        json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
            {"name": MARKED_TOOL, "inputSchema": {"type": "object", "properties": {"region": region}}},
            {"name": BROKEN_TOOL, "inputSchema": broken},
        ]}})
        .to_string()
    }

    /// The first listing leaves the region unmarked, so the first call
    /// misses the header the server wants; the second listing marks it.
    #[test]
    fn a_header_mismatch_lists_again_and_the_retry_mirrors_the_marked_argument() {
        let (calls_tx, calls) = mpsc::channel::<Req>();
        let listings = AtomicUsize::new(0);
        let base = spawn_raw(move |_| {
            move |req: Req, stream: TcpStream| match req.body["method"].as_str() {
                Some(TOOLS_LIST_METHOD) => {
                    let marked = listings.fetch_add(1, Ordering::SeqCst) > 0;
                    respond(stream, 200, &[], &tools_page(req.id(), marked));
                }
                Some(TOOLS_CALL_METHOD) => {
                    let (status, reply) = match req.header(REGION_HEADER) {
                        Some(REGION) => (200, DONE_REPLY),
                        _ => (400, HEADER_MISMATCH_REPLY),
                    };
                    let body = rpc_reply(req.id(), reply);
                    let _ = calls_tx.send(req);
                    respond(stream, status, &[], &body);
                }
                _ => respond(stream, 202, &[], ""),
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        modern(&transport);
        let tools = smol::block_on(list_tools(&transport)).unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, [MARKED_TOOL]);
        let output = smol::block_on(call_tool(
            &transport,
            MARKED_TOOL,
            &McpToolMeta::default(),
            None,
            &json!({"region": REGION}),
            &CallScope::none(),
        ))
        .unwrap();
        assert_eq!(output.model_text(), DONE);

        let calls: Vec<Req> = calls.try_iter().collect();
        let [missed, mirrored] = calls.as_slice() else {
            panic!("two calls expected, got {}", calls.len());
        };
        assert_eq!(missed.header(REGION_HEADER), None);
        assert_eq!(mirrored.header(REGION_HEADER), Some(REGION));
        assert_ne!(missed.id(), mirrored.id());
        for call in [missed, mirrored] {
            assert_eq!(call.header(NAME_HEADER), Some(MARKED_TOOL));
            assert_eq!(call.header(METHOD_HEADER), Some(TOOLS_CALL_METHOD));
            assert_eq!(call.session, None);
        }
    }

    #[test]
    fn a_modern_subscription_is_a_post_stream_closed_on_shutdown() {
        let (listens_tx, listens) = mpsc::channel::<Req>();
        let (closed_tx, closed) = mpsc::channel::<()>();
        let base = spawn_raw(move |_| {
            move |req: Req, mut stream: TcpStream| {
                if req.body["method"] != LISTEN_METHOD {
                    return respond(stream, 202, &[], "");
                }
                let id = req.id();
                let _ = listens_tx.send(req);
                open_events(&mut stream);
                send_event(
                    &mut stream,
                    &json!({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged", "params": {META_KEY: {SUBSCRIPTION_ID_META: id}, NOTIFICATIONS_KEY: {TOOLS_LIST_CHANGED_FILTER: true}}}),
                );
                send_event(
                    &mut stream,
                    &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed", "params": {META_KEY: {SUBSCRIPTION_ID_META: id}}}),
                );
                let opened = Instant::now();
                while opened.elapsed() < CLIENT_WAIT {
                    if stream.write_all(KEEPALIVE).is_err() {
                        let _ = closed_tx.send(());
                        return;
                    }
                    thread::sleep(KEEPALIVE_INTERVAL);
                }
            }
        });

        let transport = transport_with(&format!("{base}/mcp"), HashMap::new(), None);
        modern(&transport);
        let changes = transport.shared.inbound.list_changes();
        transport.listen(&ServerCapabilities {
            tools: Some(ListCapability { list_changed: true }),
            prompts: Some(ListCapability::default()),
            ..ServerCapabilities::default()
        });

        assert_eq!(changes.recv_timeout(CLIENT_WAIT), Ok(ListKind::Tools));
        assert_eq!(
            transport.shared.inbound.granted(),
            HashSet::from([ListKind::Tools])
        );
        let listen = listens.recv_timeout(CLIENT_WAIT).unwrap();
        assert_eq!(
            listen.body["params"][NOTIFICATIONS_KEY],
            json!({TOOLS_LIST_CHANGED_FILTER: true})
        );
        assert_eq!(listen.header(METHOD_HEADER), Some(LISTEN_METHOD));
        assert_eq!(listen.session, None);
        assert!(closed.try_recv().is_err(), "the stream ended early");
        smol::block_on(transport.shutdown());
        assert_eq!(closed.recv_timeout(CLIENT_WAIT), Ok(()));
    }
}
