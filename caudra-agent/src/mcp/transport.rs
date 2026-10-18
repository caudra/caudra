use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::future;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use smol::Timer;
use tracing::{debug, info, warn};

use super::McpToolMeta;
use super::call::CallScope;
use super::elicitation::ElicitAction;
use super::era::ProtocolEra;
use super::error::McpError;
use super::inbound::{ElicitParams, Inbound};
use super::kind::McpServerKind;
use super::output::McpOutput;
use super::protocol::{
    CacheHint, CallToolResult, ELICITATION_CREATE_METHOD, GetPromptResult, HEADER_MISMATCH,
    INITIALIZE_METHOD, INITIALIZED_NOTIFICATION, METHOD_NOT_FOUND, PROMPTS_GET_METHOD,
    PROMPTS_LIST_METHOD, PromptInfo, PromptMessage, ServerCapabilities, ServerDiscovery,
    TOOLS_CALL_METHOD, TOOLS_LIST_METHOD, ToolInfo, URL_ELICITATION_REQUIRED, initialize_params,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A server listing more pages than this is cut off, with a warning.
pub const MAX_LIST_PAGES: usize = 64;
/// How many times one call may come back asking for input before it fails.
pub const MAX_INPUT_ROUNDS: usize = 16;
/// How long a call waits before retrying a server that asked for nothing
/// but another try.
pub const INPUT_RETRY_BACKOFF: Duration = Duration::from_millis(500);
pub(super) const UNSUPPORTED_INPUT: &str = "which Caudra does not support";
pub(super) const INPUT_UNAVAILABLE: &str = "it needs an answer, and nobody here can be asked";
const INPUT_ROUNDS_EXCEEDED: &str = "it still wanted input after the most rounds a call may take";
const EMPTY_INPUT_REQUIRED: &str = "an input_required result asked for nothing";
const UNKNOWN_RESULT_TYPE: &str = "unknown resultType";
const RESULT_TYPE_KEY: &str = "resultType";
const RESULT_COMPLETE: &str = "complete";
const RESULT_INPUT_REQUIRED: &str = "input_required";
const INPUT_REQUESTS_KEY: &str = "inputRequests";
const INPUT_RESPONSES_KEY: &str = "inputResponses";
const REQUEST_STATE_KEY: &str = "requestState";
const METHOD_KEY: &str = "method";
const PARAMS_KEY: &str = "params";
/// How long a call waits, after the user agreed to open a URL, for the
/// server to say the browser step is done before it retries anyway.
const URL_COMPLETION_WAIT: Duration = Duration::from_secs(300);
pub(super) const URL_ELICITATION_DECLINED: &str = "the browser step it asked for was declined";
pub(super) const URL_ELICITATION_UNAVAILABLE: &str =
    "it asked for a browser step, and nobody here can be asked";
const TOOLS_FIELD: &str = "tools";
const PROMPTS_FIELD: &str = "prompts";
const CURSOR_KEY: &str = "cursor";
const NEXT_CURSOR_KEY: &str = "nextCursor";
const ELICITATIONS_KEY: &str = "elicitations";
const IS_ERROR_CODE: i64 = -1;

pub trait McpTransport: Send + Sync {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        scope: &'a CallScope,
    ) -> BoxFuture<'a, Result<Value, McpError>>;
    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>>;
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
    fn server_name(&self) -> &Arc<str>;
    fn transport_kind(&self) -> &'static str;
    fn child_pids(&self) -> Vec<u32> {
        Vec::new()
    }
    /// What the server sends unasked, for a transport that reads any.
    fn inbound(&self) -> Option<&Inbound> {
        None
    }
    fn era(&self) -> ProtocolEra {
        self.inbound().map(Inbound::era).unwrap_or_default()
    }
    /// Opens the standing stream a server pushes the list changes in
    /// `capabilities` on, where the transport has one apart from its calls.
    fn listen(&self, _capabilities: &ServerCapabilities) {}
}

pub(super) fn invalid_response(name: &Arc<str>, e: impl std::fmt::Display) -> McpError {
    McpError::InvalidResponse {
        server: (**name).into(),
        reason: e.to_string(),
    }
}

pub async fn initialize(transport: &dyn McpTransport) -> Result<ServerDiscovery, McpError> {
    let result = transport
        .send_request(
            INITIALIZE_METHOD,
            Some(initialize_params()),
            &CallScope::none(),
        )
        .await?;
    transport
        .send_notification(INITIALIZED_NOTIFICATION, None)
        .await?;
    Ok(ServerDiscovery::from_initialize(&result))
}

/// A call carried through every round of input its server asks for. Each
/// retry is a new request carrying the answers and the server's
/// `requestState` as it came; a round that asks for nothing but another try
/// waits [`INPUT_RETRY_BACKOFF`] first.
pub(super) async fn send(
    transport: &dyn McpTransport,
    method: &str,
    params: impl Fn() -> Option<Value>,
    scope: &CallScope,
    reissue: bool,
) -> Result<Value, McpError> {
    let server = transport.server_name();
    let mut input = Map::new();
    for round in 0..=MAX_INPUT_ROUNDS {
        let answered = || with_input(params(), &input);
        let mut result = send_once(transport, method, answered, scope, reissue).await?;
        match result.get(RESULT_TYPE_KEY) {
            None => return Ok(result),
            Some(Value::String(kind)) if kind == RESULT_COMPLETE => return Ok(result),
            Some(Value::String(kind)) if kind == RESULT_INPUT_REQUIRED => {}
            Some(other) => {
                return Err(invalid_response(
                    server,
                    format!("{UNKNOWN_RESULT_TYPE} {other}"),
                ));
            }
        }
        if round == MAX_INPUT_ROUNDS {
            break;
        }
        input = answer_input(transport, &mut result, scope).await?;
        if !input.contains_key(INPUT_RESPONSES_KEY) {
            future::or(
                async {
                    Timer::after(INPUT_RETRY_BACKOFF).await;
                },
                scope.cancel.cancelled(),
            )
            .await;
        }
        if scope.cancel.is_cancelled() {
            return Err(McpError::Cancelled {
                server: (**server).into(),
            });
        }
    }
    Err(McpError::Elicitation {
        server: (**server).into(),
        reason: INPUT_ROUNDS_EXCEEDED.into(),
    })
}

/// A call whose stream broke goes out again with a new id only when sending
/// it twice cannot do harm; otherwise its outcome stays unknown.
async fn send_once(
    transport: &dyn McpTransport,
    method: &str,
    params: impl Fn() -> Option<Value>,
    scope: &CallScope,
    reissue: bool,
) -> Result<Value, McpError> {
    match transport.send_request(method, params(), scope).await {
        Err(McpError::ConnectionLost { .. }) if reissue => {
            warn!(server = %transport.server_name(), method, "MCP stream broke; sending the request again");
            transport.send_request(method, params(), scope).await
        }
        result => result,
    }
}

fn with_input(params: Option<Value>, input: &Map<String, Value>) -> Option<Value> {
    if input.is_empty() {
        return params;
    }
    let mut params = params.unwrap_or_else(|| json!({}));
    if let Some(params) = params.as_object_mut() {
        params.extend(input.clone());
    }
    Some(params)
}

/// The next round's `inputResponses` and `requestState`, answering what an
/// `input_required` result asked.
async fn answer_input(
    transport: &dyn McpTransport,
    result: &mut Value,
    scope: &CallScope,
) -> Result<Map<String, Value>, McpError> {
    let mut next = Map::new();
    if let Some(Value::Object(requests)) = result.get_mut(INPUT_REQUESTS_KEY).map(Value::take) {
        let mut responses = Map::new();
        for (key, request) in requests {
            responses.insert(key, answer_request(transport, request, scope).await?);
        }
        next.insert(INPUT_RESPONSES_KEY.into(), Value::Object(responses));
    }
    if let Some(state) = result
        .get_mut(REQUEST_STATE_KEY)
        .map(Value::take)
        .filter(|state| !state.is_null())
    {
        next.insert(REQUEST_STATE_KEY.into(), state);
    }
    if next.is_empty() {
        return Err(invalid_response(
            transport.server_name(),
            EMPTY_INPUT_REQUIRED,
        ));
    }
    Ok(next)
}

/// Only elicitation is answered: Caudra never declares sampling or roots,
/// so a server that asks for them gets no retry.
async fn answer_request(
    transport: &dyn McpTransport,
    request: Value,
    scope: &CallScope,
) -> Result<Value, McpError> {
    let server = &**transport.server_name();
    let failed = |reason: String| McpError::Elicitation {
        server: server.into(),
        reason,
    };
    let method = request[METHOD_KEY].as_str().unwrap_or_default();
    if method != ELICITATION_CREATE_METHOD {
        return Err(failed(format!(
            "server asked for `{method}`, {UNSUPPORTED_INPUT}"
        )));
    }
    if scope.elicitor.is_none() {
        return Err(failed(INPUT_UNAVAILABLE.into()));
    }
    let elicitation = ElicitParams::deserialize(&request[PARAMS_KEY])
        .map_err(|error| invalid_response(transport.server_name(), error))?
        .into_request(transport.inbound().and_then(Inbound::kind))
        .map_err(|error| invalid_response(transport.server_name(), error.message))?;
    match scope.elicit(server, elicitation, future::pending()).await {
        Some(response) => Ok(response.to_result()),
        None => Err(McpError::Cancelled {
            server: server.into(),
        }),
    }
}

/// Every page of a list. An empty cursor is a cursor like any other; a
/// cursor seen before, or more than [`MAX_LIST_PAGES`] pages, ends the
/// listing with what arrived so far.
pub(super) async fn list_paged<T: DeserializeOwned>(
    transport: &dyn McpTransport,
    method: &str,
    field: &str,
) -> Result<Vec<T>, McpError> {
    let scope = CallScope::none();
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_LIST_PAGES {
        let params = || cursor.as_ref().map(|cursor| json!({CURSOR_KEY: cursor}));
        let mut page = send(transport, method, params, &scope, true).await?;
        let cache = CacheHint::parse(&page);
        debug!(server = %transport.server_name(), method, ttl_ms = cache.ttl.as_millis() as u64, cache_scope = ?cache.scope, "MCP list page");
        let batch: Vec<T> = serde_json::from_value(page[field].take())
            .map_err(|e| invalid_response(transport.server_name(), e))?;
        items.extend(batch);
        match page[NEXT_CURSOR_KEY].as_str() {
            None => return Ok(items),
            Some(next) if !seen.insert(next.to_owned()) => {
                warn!(server = %transport.server_name(), method, cursor = next, "MCP server repeated a list cursor");
                return Ok(items);
            }
            Some(next) => cursor = Some(next.to_owned()),
        }
    }
    warn!(server = %transport.server_name(), method, pages = MAX_LIST_PAGES, "MCP list cut off at the page limit");
    Ok(items)
}

pub async fn list_tools(transport: &dyn McpTransport) -> Result<Vec<ToolInfo>, McpError> {
    list_paged(transport, TOOLS_LIST_METHOD, TOOLS_FIELD).await
}

pub async fn list_prompts(transport: &dyn McpTransport) -> Result<Vec<PromptInfo>, McpError> {
    match list_paged(transport, PROMPTS_LIST_METHOD, PROMPTS_FIELD).await {
        Err(McpError::RpcError { code, .. }) if code == METHOD_NOT_FOUND => Ok(vec![]),
        result => result,
    }
}

pub async fn get_prompt(
    transport: &dyn McpTransport,
    name: &str,
    arguments: &HashMap<String, String>,
    scope: &CallScope,
) -> Result<Vec<PromptMessage>, McpError> {
    let params = || Some(json!({ "name": name, "arguments": arguments }));
    let result = send(transport, PROMPTS_GET_METHOD, params, scope, true).await?;
    let parsed: GetPromptResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(parsed.messages)
}

pub async fn call_tool(
    transport: &dyn McpTransport,
    tool_name: &str,
    meta: &McpToolMeta,
    kind: Option<McpServerKind>,
    args: &Value,
    scope: &CallScope,
) -> Result<McpOutput, McpError> {
    let server = &**transport.server_name();
    let start = Instant::now();
    let params = || Some(json!({"name": tool_name, "arguments": args}));
    let reissue = meta.annotations.idempotent();
    let result = match send(transport, TOOLS_CALL_METHOD, params, scope, reissue).await {
        Err(
            error @ McpError::RpcError {
                code: URL_ELICITATION_REQUIRED,
                ..
            },
        ) => {
            complete_url_elicitations(transport, kind, error, scope).await?;
            send(transport, TOOLS_CALL_METHOD, params, scope, reissue).await
        }
        Err(McpError::RpcError {
            code: HEADER_MISMATCH,
            ..
        }) => {
            info!(
                server,
                tool = tool_name,
                "MCP header mismatch; listing tools again"
            );
            list_tools(transport).await?;
            send(transport, TOOLS_CALL_METHOD, params, scope, reissue).await
        }
        result => result,
    }?;
    let call_result: CallToolResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;

    info!(
        server,
        tool = tool_name,
        is_error = call_result.is_error,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tools/call response"
    );
    let mut output = McpOutput::from_result(call_result);
    if !output.is_error {
        output.schema_mismatch = meta
            .output_schema
            .as_ref()
            .zip(output.structured_content())
            .and_then(|(schema, structured)| {
                schema_mismatch(server, tool_name, schema, structured)
            });
    }
    output.kind = kind;
    output.title = meta.title().map(str::to_owned);
    output.interactions = scope.take_interactions();
    Ok(output)
}

/// What a result its tool flagged `isError` reads as to the model: the error
/// such a result failed the call with before its output was kept.
pub fn reported_error(server: &str, output: &McpOutput) -> McpError {
    McpError::RpcError {
        server: server.into(),
        code: IS_ERROR_CODE,
        message: output.model_text(),
        data: None,
    }
}

/// A legacy server's -32042: the call needs the user to finish something in
/// a browser first. Each URL goes through the call's elicitor; once agreed,
/// the server's completion notice, or a long wait, clears the way to retry.
async fn complete_url_elicitations(
    transport: &dyn McpTransport,
    kind: Option<McpServerKind>,
    error: McpError,
    scope: &CallScope,
) -> Result<(), McpError> {
    let McpError::RpcError {
        server,
        data: Some(data),
        ..
    } = &error
    else {
        return Err(error);
    };
    let elicitations =
        match serde_json::from_value::<Vec<ElicitParams>>(data[ELICITATIONS_KEY].clone()) {
            Ok(elicitations) if elicitations.iter().any(|e| e.url.is_some()) => elicitations,
            _ => return Err(error),
        };
    let refused = |reason: &str| McpError::Elicitation {
        server: server.clone(),
        reason: reason.into(),
    };
    for params in elicitations {
        let elicitation_id = params.elicitation_id.clone();
        let request = params
            .into_request(kind)
            .map_err(|error| refused(&error.message))?;
        let response = scope.elicit(server, request, future::pending()).await;
        if scope.cancel.is_cancelled() {
            return Err(McpError::Cancelled {
                server: server.clone(),
            });
        }
        match response.map(|response| response.action) {
            Some(ElicitAction::Accept) => {
                if let (Some(inbound), Some(id)) = (transport.inbound(), elicitation_id) {
                    let completed =
                        future::or(inbound.completion(&id, URL_COMPLETION_WAIT), async {
                            scope.cancel.cancelled().await;
                            false
                        })
                        .await;
                    if completed {
                        scope.url_completed(server, &id);
                    }
                }
            }
            Some(_) => return Err(refused(URL_ELICITATION_DECLINED)),
            None => return Err(refused(URL_ELICITATION_UNAVAILABLE)),
        }
    }
    if scope.cancel.is_cancelled() {
        return Err(McpError::Cancelled {
            server: server.clone(),
        });
    }
    Ok(())
}

/// Why `structured` breaks the tool's declared `outputSchema`. A schema that
/// does not compile checks nothing; either way the call still succeeds.
fn schema_mismatch(server: &str, tool: &str, schema: &Value, structured: &Value) -> Option<String> {
    let validator = match jsonschema::options().offline().build(schema) {
        Ok(validator) => validator,
        Err(error) => {
            warn!(server, tool, %error, "MCP outputSchema does not compile");
            return None;
        }
    };
    let error = validator.validate(structured).err()?.to_string();
    warn!(server, tool, %error, "MCP structuredContent does not match its outputSchema");
    Some(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::mcp::call::{ElicitRequest, ElicitResponse, Elicitor};
    use crate::mcp::error::CONNECTION_LOST;
    use crate::types::Answerer;
    use std::sync::Mutex;
    use test_case::test_case;

    const SERVER: &str = "pages";
    const TOOL: &str = "t";
    const TITLE: &str = "Run a script";
    const TEXT: &str = "done";
    /// What the model read for an `isError` result when the call failed on it.
    const LEGACY_IS_ERROR_TEXT: &str = "server pages returned error -1: done\ndone";
    const UNCHECKED_ERROR: &str = "an error result is not held to the tool's outputSchema";

    /// Serves `tools/list` from `pages`, keyed by the cursor asked for, and
    /// remembers each cursor in the order it came.
    struct PagedTransport {
        name: Arc<str>,
        pages: HashMap<Option<String>, Value>,
        asked: Mutex<Vec<Option<String>>>,
    }

    impl PagedTransport {
        fn new(pages: Vec<(Option<&str>, Value)>) -> Self {
            Self {
                name: Arc::from(SERVER),
                pages: pages
                    .into_iter()
                    .map(|(cursor, page)| (cursor.map(str::to_owned), page))
                    .collect(),
                asked: Mutex::default(),
            }
        }
    }

    impl McpTransport for PagedTransport {
        fn send_request<'a>(
            &'a self,
            _method: &'a str,
            params: Option<Value>,
            _scope: &'a CallScope,
        ) -> BoxFuture<'a, Result<Value, McpError>> {
            let cursor = params.and_then(|params| params[CURSOR_KEY].as_str().map(str::to_owned));
            self.asked.lock().unwrap().push(cursor.clone());
            let page = self.pages.get(&cursor).cloned().unwrap_or_else(|| {
                json!({TOOLS_FIELD: [], NEXT_CURSOR_KEY: format!("{}x", cursor.unwrap_or_default())})
            });
            Box::pin(async move { Ok(page) })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            SERVER
        }
    }

    fn page(tools: &[&str], next: Option<&str>) -> Value {
        let tools: Vec<Value> = tools.iter().map(|name| json!({"name": name})).collect();
        match next {
            Some(next) => json!({TOOLS_FIELD: tools, NEXT_CURSOR_KEY: next}),
            None => json!({TOOLS_FIELD: tools}),
        }
    }

    #[test_case(vec![(None, page(&["a"], Some("c1"))), (Some("c1"), page(&["b"], None))], &["a", "b"], 2 ; "follows_cursors")]
    #[test_case(vec![(None, page(&["a"], Some(""))), (Some(""), page(&["b"], None))], &["a", "b"], 2 ; "empty_cursor_is_a_cursor")]
    #[test_case(vec![(None, page(&["a"], Some("c1"))), (Some("c1"), page(&["b"], Some("c1")))], &["a", "b"], 2 ; "repeated_cursor_stops")]
    #[test_case(vec![(None, page(&[], Some("x")))], &[], MAX_LIST_PAGES ; "page_limit_stops")]
    fn tools_list_follows_every_page(
        pages: Vec<(Option<&str>, Value)>,
        expected: &[&str],
        requests: usize,
    ) {
        let transport = PagedTransport::new(pages);
        let tools = smol::block_on(list_tools(&transport)).unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, expected);
        assert_eq!(transport.asked.lock().unwrap().len(), requests);
    }

    struct Answering(ElicitAction);

    impl Elicitor for Answering {
        fn elicit<'a>(
            &'a self,
            _server: &'a str,
            _request: ElicitRequest,
        ) -> BoxFuture<'a, ElicitResponse> {
            Box::pin(async move {
                ElicitResponse {
                    action: self.0,
                    content: None,
                    by: Answerer::User,
                    answers: Vec::new(),
                }
            })
        }
    }

    fn url_required() -> McpError {
        McpError::RpcError {
            server: SERVER.into(),
            code: URL_ELICITATION_REQUIRED,
            message: SERVER.into(),
            data: Some(Box::new(
                json!({ELICITATIONS_KEY: [{"mode": "url", "url": "https://example.com", "message": "Sign in", "elicitationId": "e1"}]}),
            )),
        }
    }

    #[test_case(Some(ElicitAction::Decline), URL_ELICITATION_DECLINED ; "declined")]
    #[test_case(Some(ElicitAction::Cancel), URL_ELICITATION_DECLINED ; "cancelled")]
    #[test_case(None, URL_ELICITATION_UNAVAILABLE ; "nobody_to_ask")]
    fn a_refused_browser_step_fails_the_call(action: Option<ElicitAction>, reason: &str) {
        let transport = PagedTransport::new(Vec::new());
        let elicitor = action.map(|action| Arc::new(Answering(action)) as Arc<dyn Elicitor>);
        let scope = CallScope::new(elicitor, None, CancelToken::none());
        let result = smol::block_on(complete_url_elicitations(
            &transport,
            None,
            url_required(),
            &scope,
        ));
        assert!(
            matches!(&result, Err(McpError::Elicitation { reason: got, .. }) if got == reason),
            "{result:?}"
        );
        assert_eq!(
            scope.take_interactions().len(),
            usize::from(action.is_some())
        );
    }

    #[test]
    fn an_error_without_urls_stays_the_servers_error() {
        let transport = PagedTransport::new(Vec::new());
        let error = McpError::RpcError {
            server: SERVER.into(),
            code: URL_ELICITATION_REQUIRED,
            message: SERVER.into(),
            data: Some(Box::new(json!({ELICITATIONS_KEY: []}))),
        };
        let result = smol::block_on(complete_url_elicitations(
            &transport,
            None,
            error,
            &CallScope::none(),
        ));
        assert!(matches!(result, Err(McpError::RpcError { .. })));
    }

    /// Answers each request with the next of `replies`, and keeps the
    /// params each came with.
    struct ScriptedTransport {
        name: Arc<str>,
        replies: Mutex<Vec<Result<Value, McpError>>>,
        sent: Mutex<Vec<Value>>,
    }

    impl ScriptedTransport {
        fn new(mut replies: Vec<Result<Value, McpError>>) -> Self {
            replies.reverse();
            Self {
                name: Arc::from(SERVER),
                replies: Mutex::new(replies),
                sent: Mutex::default(),
            }
        }

        fn sent(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn params(&self, request: usize) -> Value {
            self.sent.lock().unwrap()[request].clone()
        }
    }

    impl McpTransport for ScriptedTransport {
        fn send_request<'a>(
            &'a self,
            _method: &'a str,
            params: Option<Value>,
            _scope: &'a CallScope,
        ) -> BoxFuture<'a, Result<Value, McpError>> {
            self.sent.lock().unwrap().push(params.unwrap_or_default());
            let reply = self.replies.lock().unwrap().pop().expect("no reply left");
            Box::pin(async move { reply })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            SERVER
        }
    }

    fn text_result() -> Value {
        json!({"content": [{"type": "text", "text": TEXT}, {"type": "text", "text": TEXT}]})
    }

    fn lost() -> McpError {
        McpError::ConnectionLost {
            server: SERVER.into(),
        }
    }

    fn call(
        transport: &ScriptedTransport,
        meta: &McpToolMeta,
        scope: &CallScope,
    ) -> Result<McpOutput, McpError> {
        smol::block_on(call_tool(
            transport,
            TOOL,
            meta,
            Some(McpServerKind::Executor),
            &json!({}),
            scope,
        ))
    }

    #[test]
    fn a_call_carries_the_tools_kind_title_and_unchanged_model_text() {
        let transport = ScriptedTransport::new(vec![Ok(text_result())]);
        let meta = McpToolMeta {
            title: Some(TITLE.into()),
            ..McpToolMeta::default()
        };
        let output = call(&transport, &meta, &CallScope::none()).unwrap();
        let legacy: CallToolResult = serde_json::from_value(text_result()).unwrap();
        assert_eq!(output.model_text(), legacy.joined_text());
        assert_eq!(output.kind, Some(McpServerKind::Executor));
        assert_eq!(output.title.as_deref(), Some(TITLE));
        assert!(output.interactions.is_empty());
    }

    #[test]
    fn a_result_flagged_is_error_is_kept_and_reads_as_the_legacy_error() {
        let result = json!({
            "content": [{"type": "text", "text": TEXT}, {"type": "text", "text": TEXT}],
            "structuredContent": {"status": "error"},
            "isError": true,
        });
        let transport = ScriptedTransport::new(vec![Ok(result)]);
        let meta = McpToolMeta {
            output_schema: Some(json!({"type": "object", "required": ["result"]})),
            ..McpToolMeta::default()
        };
        let output = call(&transport, &meta, &CallScope::none()).unwrap();
        assert!(output.is_error);
        assert_eq!(output.kind, Some(McpServerKind::Executor));
        assert_eq!(output.schema_mismatch, None, "{UNCHECKED_ERROR}");
        assert_eq!(
            reported_error(SERVER, &output).to_string(),
            LEGACY_IS_ERROR_TEXT
        );
    }

    #[test]
    fn an_accepted_browser_step_retries_the_call_and_is_recorded() {
        let transport = ScriptedTransport::new(vec![Err(url_required()), Ok(text_result())]);
        let scope = CallScope::new(
            Some(Arc::new(Answering(ElicitAction::Accept))),
            None,
            CancelToken::none(),
        );
        let output = call(&transport, &McpToolMeta::default(), &scope).unwrap();
        assert_eq!(transport.sent(), 2);
        let [interaction] = output.interactions.as_slice() else {
            panic!("one interaction expected: {:?}", output.interactions);
        };
        assert_eq!(interaction.action, ElicitAction::Accept);
        assert!(interaction.url.is_some());
    }

    #[test_case(json!({"readOnlyHint": true}), true ; "read_only_reissued")]
    #[test_case(json!({"idempotentHint": true}), true ; "idempotent_reissued")]
    #[test_case(json!({}), false ; "unannotated_outcome_unknown")]
    fn a_broken_stream_reissues_only_a_safe_call(annotations: Value, reissued: bool) {
        let transport = ScriptedTransport::new(vec![Err(lost()), Ok(text_result())]);
        let meta = McpToolMeta {
            annotations: serde_json::from_value(annotations).unwrap(),
            ..McpToolMeta::default()
        };
        let result = call(&transport, &meta, &CallScope::none());
        assert_eq!(transport.sent(), if reissued { 2 } else { 1 });
        assert_eq!(result.is_ok(), reissued);
        if let Err(error) = result {
            assert!(error.to_string().contains(CONNECTION_LOST), "{error}");
        }
    }

    #[test_case(json!({"count": 1}), None ; "matches")]
    #[test_case(json!({"count": "one"}), Some(()) ; "wrong_type")]
    fn structured_content_is_checked_against_the_output_schema(
        structured: Value,
        mismatch: Option<()>,
    ) {
        let schema = json!({"type": "object", "properties": {"count": {"type": "integer"}}});
        assert_eq!(
            schema_mismatch(SERVER, TOOL, &schema, &structured).map(|_| ()),
            mismatch
        );
    }

    #[test_case(json!({"count": 1}), false; "valid_local_reference")]
    #[test_case(json!({"count": "one"}), true; "invalid_local_reference")]
    fn structured_content_checks_local_schema_references(structured: Value, mismatch: bool) {
        let schema = json!({
            "$defs": { "count": { "type": "integer" } },
            "properties": { "count": { "$ref": "#/$defs/count" } },
        });
        assert_eq!(
            schema_mismatch(SERVER, TOOL, &schema, &structured).is_some(),
            mismatch
        );
    }

    #[test_case("#/missing"; "missing_local_reference")]
    #[test_case("http://127.0.0.1:9/schema.json"; "http_reference")]
    #[test_case("https://127.0.0.1:9/schema.json"; "https_reference")]
    #[test_case("file:///schema.json"; "file_reference")]
    fn unresolvable_output_schema_still_does_not_fail_the_call(reference: &str) {
        let schema = json!({ "$ref": reference });
        assert_eq!(schema_mismatch(SERVER, TOOL, &schema, &json!({})), None);
    }

    const INPUT_KEY: &str = "login";
    const REQUEST_STATE: &str = "état \"opaque\" \\ \u{1F600} =?base64?x?=";
    const SAMPLING_METHOD: &str = "sampling/createMessage";

    fn elicitation_request() -> Value {
        json!({
            METHOD_KEY: ELICITATION_CREATE_METHOD,
            PARAMS_KEY: {
                "mode": "form",
                "message": "Who are you?",
                "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}},
            },
        })
    }

    fn input_required(request: Option<Value>, state: Option<&str>) -> Value {
        let mut result = json!({RESULT_TYPE_KEY: RESULT_INPUT_REQUIRED});
        if let Some(request) = request {
            result[INPUT_REQUESTS_KEY] = json!({INPUT_KEY: request});
        }
        if let Some(state) = state {
            result[REQUEST_STATE_KEY] = state.into();
        }
        result
    }

    fn answering(action: ElicitAction) -> CallScope {
        CallScope::new(Some(Arc::new(Answering(action))), None, CancelToken::none())
    }

    #[test_case(ElicitAction::Accept ; "accept")]
    #[test_case(ElicitAction::Decline ; "decline")]
    fn an_answered_round_retries_with_the_answer_and_the_state(action: ElicitAction) {
        let transport = ScriptedTransport::new(vec![
            Ok(input_required(
                Some(elicitation_request()),
                Some(REQUEST_STATE),
            )),
            Ok(text_result()),
        ]);
        let output = call(&transport, &McpToolMeta::default(), &answering(action)).unwrap();
        assert_eq!(transport.sent(), 2);
        assert!(transport.params(0).get(INPUT_RESPONSES_KEY).is_none());
        let retry = transport.params(1);
        assert_eq!(retry["name"], TOOL);
        assert_eq!(
            retry[INPUT_RESPONSES_KEY],
            json!({INPUT_KEY: {"action": action}})
        );
        assert_eq!(retry[REQUEST_STATE_KEY], REQUEST_STATE);
        let [interaction] = output.interactions.as_slice() else {
            panic!("one interaction expected: {:?}", output.interactions);
        };
        assert_eq!(interaction.action, action);
    }

    #[test]
    fn a_round_with_only_state_waits_then_echoes_it_alone() {
        let transport = ScriptedTransport::new(vec![
            Ok(input_required(None, Some(REQUEST_STATE))),
            Ok(text_result()),
        ]);
        let started = Instant::now();
        call(&transport, &McpToolMeta::default(), &CallScope::none()).unwrap();
        assert!(started.elapsed() >= INPUT_RETRY_BACKOFF);
        let retry = transport.params(1);
        assert_eq!(retry[REQUEST_STATE_KEY], REQUEST_STATE);
        assert!(retry.get(INPUT_RESPONSES_KEY).is_none());
    }

    #[test]
    fn a_cancel_between_rounds_ends_the_call() {
        let transport = ScriptedTransport::new(vec![Ok(input_required(None, Some(REQUEST_STATE)))]);
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let result = call(
            &transport,
            &McpToolMeta::default(),
            &CallScope::new(None, None, cancel),
        );
        assert!(
            matches!(result, Err(McpError::Cancelled { .. })),
            "{result:?}"
        );
        assert_eq!(transport.sent(), 1);
    }

    #[test]
    fn a_server_that_keeps_asking_is_cut_off() {
        let rounds = (0..=MAX_INPUT_ROUNDS)
            .map(|_| Ok(input_required(Some(elicitation_request()), None)))
            .collect();
        let transport = ScriptedTransport::new(rounds);
        let scope = answering(ElicitAction::Accept);
        let result = call(&transport, &McpToolMeta::default(), &scope);
        assert!(
            matches!(&result, Err(McpError::Elicitation { reason, .. }) if reason == INPUT_ROUNDS_EXCEEDED),
            "{result:?}"
        );
        assert_eq!(transport.sent(), MAX_INPUT_ROUNDS + 1);
        assert_eq!(scope.take_interactions().len(), MAX_INPUT_ROUNDS);
    }

    #[test_case(SAMPLING_METHOD ; "sampling")]
    #[test_case("roots/list" ; "roots")]
    #[test_case("vendor/unknown" ; "unknown")]
    fn an_unsupported_input_request_fails_without_a_retry(method: &str) {
        let transport = ScriptedTransport::new(vec![Ok(input_required(
            Some(json!({METHOD_KEY: method, PARAMS_KEY: {}})),
            Some(REQUEST_STATE),
        ))]);
        let result = call(
            &transport,
            &McpToolMeta::default(),
            &answering(ElicitAction::Accept),
        );
        let expected = format!("server asked for `{method}`, {UNSUPPORTED_INPUT}");
        assert!(
            matches!(&result, Err(McpError::Elicitation { reason, .. }) if *reason == expected),
            "{result:?}"
        );
        assert_eq!(transport.sent(), 1);
    }

    #[test]
    fn an_elicitation_nobody_can_answer_fails_the_call() {
        let transport =
            ScriptedTransport::new(vec![Ok(input_required(Some(elicitation_request()), None))]);
        let result = call(&transport, &McpToolMeta::default(), &CallScope::none());
        assert!(
            matches!(&result, Err(McpError::Elicitation { reason, .. }) if reason == INPUT_UNAVAILABLE),
            "{result:?}"
        );
        assert_eq!(transport.sent(), 1);
    }

    #[test_case(None, true ; "absent_is_complete")]
    #[test_case(Some(json!(RESULT_COMPLETE)), true ; "complete")]
    #[test_case(Some(json!("partial")), false ; "unknown_type")]
    #[test_case(Some(json!(1)), false ; "not_a_string")]
    fn the_result_type_decides_what_a_result_is(result_type: Option<Value>, complete: bool) {
        let mut result = text_result();
        if let Some(result_type) = result_type {
            result[RESULT_TYPE_KEY] = result_type;
        }
        let transport = ScriptedTransport::new(vec![Ok(result)]);
        let result = call(&transport, &McpToolMeta::default(), &CallScope::none());
        match complete {
            true => assert!(result.is_ok(), "{result:?}"),
            false => assert!(
                matches!(result, Err(McpError::InvalidResponse { .. })),
                "{result:?}"
            ),
        }
    }

    #[test]
    fn input_required_asking_for_nothing_is_invalid() {
        let transport = ScriptedTransport::new(vec![Ok(input_required(None, None))]);
        let result = call(&transport, &McpToolMeta::default(), &CallScope::none());
        assert!(
            matches!(&result, Err(McpError::InvalidResponse { reason, .. }) if reason == EMPTY_INPUT_REQUIRED),
            "{result:?}"
        );
    }

    #[test]
    fn a_header_mismatch_lists_the_tools_again_and_retries_once() {
        let mismatch = McpError::RpcError {
            server: SERVER.into(),
            code: HEADER_MISMATCH,
            message: SERVER.into(),
            data: None,
        };
        let transport = ScriptedTransport::new(vec![
            Err(mismatch),
            Ok(page(&[TOOL], None)),
            Ok(text_result()),
        ]);
        call(&transport, &McpToolMeta::default(), &CallScope::none()).unwrap();
        assert_eq!(transport.sent(), 3);
        assert_eq!(transport.params(2)["name"], TOOL);
    }
}
