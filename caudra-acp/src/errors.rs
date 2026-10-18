use std::fmt::Display;

use agent_client_protocol_schema::v1::{Error as AcpError, ErrorCode, SessionId};
use caudra_providers::UserFailure;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) const INITIALIZE_FIRST: &str = "initialize must be called first";
pub(crate) const PROMPT_ACTIVE: &str = "session already has an active prompt";
pub(crate) const STEER_COMPACT: &str = "a running turn cannot be steered with /compact";
pub(crate) const REQUEST_CANCELLED: &str = "request cancelled";
pub(crate) const UNKNOWN_SESSION: &str = "unknown session";
pub(crate) const SESSION_ENDED: &str = "session ended";
pub(crate) const SHUTTING_DOWN: &str = "the ACP server is shutting down";
pub(crate) const LINE_TOO_LONG: &str = "message exceeds the stdin line limit";
pub(crate) const AUTH_REQUIRED_DETAILS: &str =
    "The provider rejected its credentials. Run `caudra auth login`, then send the prompt again.";
/// Where a failure's human text lives in `data`, beside `message`.
pub(crate) const DATA_DETAILS: &str = "details";
pub(crate) const DATA_KIND: &str = "kind";
pub(crate) const RATE_LIMIT_KIND: &str = "rate_limit";

pub(crate) fn json_str(e: &(impl Display + ?Sized)) -> Value {
    Value::String(e.to_string())
}

/// The text rides in `message` and in `data.details`, so a client reading
/// either shows what went wrong rather than a bare "Internal error".
fn described(code: ErrorCode, text: &str, data: Value) -> AcpError {
    AcpError::new(code.into(), text).data(data)
}

pub(crate) fn internal(e: &(impl Display + ?Sized)) -> AcpError {
    let text = e.to_string();
    described(
        ErrorCode::InternalError,
        &text,
        json!({ DATA_DETAILS: text }),
    )
}

pub(crate) fn auth_required(text: &str) -> AcpError {
    described(ErrorCode::AuthRequired, text, json!({ DATA_DETAILS: text }))
}

/// A failure the agent or its provider reported, coded for what the client
/// can do about it: sign in again, wait out a rate limit, or neither.
pub(crate) fn agent_failure(text: &str) -> AcpError {
    match UserFailure::of(text) {
        Some(UserFailure::Auth) => auth_required(text),
        Some(UserFailure::RateLimit) => described(
            ErrorCode::InternalError,
            text,
            json!({ DATA_DETAILS: text, DATA_KIND: RATE_LIMIT_KIND }),
        ),
        None => internal(text),
    }
}

pub(crate) fn invalid_params(e: &(impl Display + ?Sized)) -> AcpError {
    AcpError::invalid_params().data(json_str(e))
}

/// Invalid params whose `message` says what was wrong, for a target the
/// client named that the session does not have.
pub(crate) fn invalid_target(text: &str) -> AcpError {
    described(
        ErrorCode::InvalidParams,
        text,
        json!({ DATA_DETAILS: text }),
    )
}

pub(crate) fn request_cancelled() -> AcpError {
    AcpError::new(ErrorCode::RequestCancelled.into(), REQUEST_CANCELLED)
}

pub(crate) fn invalid_request(message: &str) -> AcpError {
    AcpError::new(ErrorCode::InvalidRequest.into(), message)
}

pub(crate) fn unknown_session(id: &SessionId) -> AcpError {
    AcpError::new(
        ErrorCode::ResourceNotFound.into(),
        format!("{UNKNOWN_SESSION}: {id}"),
    )
}

pub(crate) fn session_ended() -> AcpError {
    AcpError::new(ErrorCode::InternalError.into(), SESSION_ENDED)
}

pub(crate) fn params(raw: &Value) -> &Value {
    raw.get("params").unwrap_or(&Value::Null)
}

pub(crate) fn parse_params<T: DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    T::deserialize(params(raw)).map_err(|e| invalid_params(&e))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const LOGIN_MISSING: &str = "no saved credentials for 'xai' — run `caudra auth login xai`";
    const REJECTED: &str =
        "authentication failed: invalid x-api-key, run `caudra auth login` or check your API key";
    const RATE_LIMITED: &str = "rate limited: rate_limit_error: slow down (retry after 2m)";
    const BROKEN: &str = "server error (500): upstream exploded";
    const AUTH_REQUIRED_WIRE_CODE: i32 = -32000;

    #[test_case(LOGIN_MISSING, ErrorCode::AuthRequired, None ; "missing_credentials_ask_for_sign_in")]
    #[test_case(REJECTED, ErrorCode::AuthRequired, None ; "rejected_credentials_ask_for_sign_in")]
    #[test_case(RATE_LIMITED, ErrorCode::InternalError, Some(RATE_LIMIT_KIND) ; "rate_limit_is_marked")]
    #[test_case(BROKEN, ErrorCode::InternalError, None ; "other_failures_stay_internal")]
    fn an_agent_failure_carries_its_text_and_code(text: &str, code: ErrorCode, kind: Option<&str>) {
        let error = agent_failure(text);

        assert_eq!(error.code, code);
        assert_eq!(error.message, text);
        let data = error.data.expect("a failure carries data");
        assert_eq!(data[DATA_DETAILS], text);
        assert_eq!(data.get(DATA_KIND).and_then(Value::as_str), kind);
    }

    /// Clients read the number, not the name: -32000 is what starts sign-in.
    #[test]
    fn an_auth_failure_reaches_the_wire_as_auth_required() {
        let wire = serde_json::to_value(agent_failure(REJECTED)).unwrap();
        assert_eq!(wire["code"], AUTH_REQUIRED_WIRE_CODE);
    }
}
