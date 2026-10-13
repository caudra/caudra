//! What one inbound JSON-RPC message is. stdio and HTTP both read a mix of
//! responses to Caudra's calls, requests from the server, and notifications,
//! and an id alone does not tell them apart: a server's own request may reuse
//! an id Caudra is waiting on.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};

use super::error::McpError;
use super::protocol::JsonRpcError;

const JSONRPC_VERSION: &str = "2.0";
const NO_RESULT_OR_ERROR: &str = "response has neither result nor error";

/// A JSON-RPC id as the peer wrote it. Caudra numbers its own requests, but a
/// server may number or name its requests however it likes, and a reply must
/// echo the id exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(Number),
    String(String),
}

impl RequestId {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(number) => number.as_u64(),
            Self::String(_) => None,
        }
    }

    fn from_value(value: Value) -> Option<Self> {
        match value {
            Value::Number(number) => Some(Self::Number(number)),
            Value::String(text) => Some(Self::String(text)),
            _ => None,
        }
    }
}

impl From<u64> for RequestId {
    fn from(id: u64) -> Self {
        Self::Number(id.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Response(Response),
    Request(Request),
    Notification(Notification),
}

/// An answer to one of Caudra's requests. `id` is `None` for an error the
/// server could not attribute, such as a parse error.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: Option<RequestId>,
    pub body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Result(Value),
    Error(JsonRpcError),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

impl Response {
    pub fn into_result(self, server: &str) -> Result<Value, McpError> {
        match self.body {
            ResponseBody::Result(result) => Ok(result),
            ResponseBody::Error(error) => Err(McpError::rpc(server, error)),
            ResponseBody::Invalid(reason) => Err(McpError::InvalidResponse {
                server: server.into(),
                reason,
            }),
        }
    }
}

/// `None` for anything that is not a JSON-RPC message at all.
pub fn classify(value: Value) -> Option<Message> {
    let Value::Object(mut object) = value else {
        return None;
    };
    let id = object.remove("id").and_then(RequestId::from_value);
    if let Some(method) = object.remove("method") {
        let Value::String(method) = method else {
            return None;
        };
        let params = object.remove("params").unwrap_or(Value::Null);
        return Some(match id {
            Some(id) => Message::Request(Request { id, method, params }),
            None => Message::Notification(Notification { method, params }),
        });
    }
    let body = response_body(&mut object)?;
    if id.is_none() && !matches!(body, ResponseBody::Error(_)) {
        return None;
    }
    Some(Message::Response(Response { id, body }))
}

/// A `null` error next to a result is a sloppy success, not a failure.
fn response_body(object: &mut Map<String, Value>) -> Option<ResponseBody> {
    let error = object.remove("error").filter(|error| !error.is_null());
    let result = object.remove("result");
    Some(match (error, result) {
        (Some(error), _) => match serde_json::from_value(error) {
            Ok(error) => ResponseBody::Error(error),
            Err(error) => ResponseBody::Invalid(error.to_string()),
        },
        (None, Some(result)) => ResponseBody::Result(result),
        (None, None) if object.contains_key("jsonrpc") => {
            ResponseBody::Invalid(NO_RESULT_OR_ERROR.into())
        }
        (None, None) => return None,
    })
}

pub fn parse(line: &str) -> Option<Message> {
    classify(serde_json::from_str(line).ok()?)
}

/// The response to a server's request, ready to send.
pub fn reply(id: &RequestId, outcome: Result<Value, JsonRpcError>) -> Value {
    match outcome {
        Ok(result) => json!({"jsonrpc": JSONRPC_VERSION, "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": JSONRPC_VERSION, "id": id, "error": error}),
    }
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": JSONRPC_VERSION, "method": method, "params": params})
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PING: &str = "ping";
    const PROGRESS: &str = "notifications/progress";

    fn response(id: Option<RequestId>, body: ResponseBody) -> Option<Message> {
        Some(Message::Response(Response { id, body }))
    }

    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "result": {}}), response(Some(1.into()), ResponseBody::Result(json!({}))) ; "result")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "result": null}), response(Some(1.into()), ResponseBody::Result(Value::Null)) ; "null_result")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": null}), response(Some(1.into()), ResponseBody::Result(json!({}))) ; "null_error_next_to_result")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "bad"}}), response(Some(1.into()), ResponseBody::Error(JsonRpcError::new(-32600, "bad"))) ; "error")]
    #[test_case(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse"}}), response(None, ResponseBody::Error(JsonRpcError::new(-32700, "parse"))) ; "unattributed_error")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1}), response(Some(1.into()), ResponseBody::Invalid(NO_RESULT_OR_ERROR.into())) ; "neither_result_nor_error")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "method": PING}), Some(Message::Request(Request { id: 1.into(), method: PING.into(), params: Value::Null })) ; "numbered_request")]
    #[test_case(json!({"jsonrpc": "2.0", "id": "a", "method": PING, "params": {}}), Some(Message::Request(Request { id: RequestId::String("a".into()), method: PING.into(), params: json!({}) })) ; "named_request")]
    #[test_case(json!({"jsonrpc": "2.0", "method": PROGRESS, "params": {"progress": 1}}), Some(Message::Notification(Notification { method: PROGRESS.into(), params: json!({"progress": 1}) })) ; "notification")]
    #[test_case(json!({"jsonrpc": "2.0", "id": null, "method": PROGRESS}), Some(Message::Notification(Notification { method: PROGRESS.into(), params: Value::Null })) ; "null_id_is_a_notification")]
    #[test_case(json!({"jsonrpc": "2.0", "id": 1, "method": 7}), None ; "non_string_method")]
    #[test_case(json!({"jsonrpc": "2.0", "result": {}}), None ; "result_without_id")]
    #[test_case(json!({"id": 1}), None ; "not_json_rpc")]
    #[test_case(json!([1, 2]), None ; "not_an_object")]
    fn classifies_every_message_shape(raw: Value, expected: Option<Message>) {
        assert_eq!(classify(raw), expected);
    }

    #[test]
    fn a_malformed_error_is_an_invalid_response() {
        let message = classify(json!({"jsonrpc": "2.0", "id": 1, "error": "nope"}));
        assert!(matches!(
            message,
            Some(Message::Response(Response {
                body: ResponseBody::Invalid(_),
                ..
            }))
        ));
    }

    #[test_case(json!(7), Some(7) ; "number")]
    #[test_case(json!("7"), None ; "string")]
    #[test_case(json!(-1), None ; "negative")]
    fn only_unsigned_numbers_name_a_caudra_request(raw: Value, expected: Option<u64>) {
        let id = RequestId::from_value(raw).unwrap();
        assert_eq!(id.as_u64(), expected);
    }

    #[test]
    fn a_reply_echoes_the_id_exactly() {
        let id = RequestId::String("server-1".into());
        assert_eq!(
            reply(&id, Ok(json!({}))),
            json!({"jsonrpc": "2.0", "id": "server-1", "result": {}})
        );
        assert_eq!(
            reply(&id, Err(JsonRpcError::new(-32601, "no"))),
            json!({"jsonrpc": "2.0", "id": "server-1", "error": {"code": -32601, "message": "no"}})
        );
    }
}
