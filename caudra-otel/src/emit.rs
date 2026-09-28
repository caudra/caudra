//! The caudra-facing API. Every function is a no-op when telemetry is off, and
//! nothing here ever returns an error: telemetry must not change what the
//! agent does.

use std::time::Duration;

use crate::attr::AttrSet;
use crate::handle;
use crate::logs::{
    EVENT_API_ERROR, EVENT_API_REQUEST, EVENT_DECISION, EVENT_TOOL_DECISION, EVENT_TOOL_RESULT,
    EVENT_USER_PROMPT,
};
use crate::metrics::{
    ACTIVE_TIME, COMMIT_COUNT, COST_USAGE, LINES_OF_CODE, PULL_REQUEST_COUNT, SESSION_COUNT,
    TOKEN_USAGE, TOOL_DECISION, Value,
};

pub const START_FRESH: &str = "fresh";
pub const START_RESUME: &str = "resume";
pub const START_CONTINUE: &str = "continue";
pub const START_FORK: &str = "fork";

pub const TOKEN_INPUT: &str = "input";
pub const TOKEN_OUTPUT: &str = "output";
pub const TOKEN_CACHE_READ: &str = "cacheRead";
pub const TOKEN_CACHE_CREATION: &str = "cacheCreation";

pub const LINES_ADDED: &str = "added";
pub const LINES_REMOVED: &str = "removed";

pub const DECISION_ACCEPT: &str = "accept";
pub const DECISION_REJECT: &str = "reject";

pub const ACTIVE_TIME_CLI: &str = "cli";

const KEY_TYPE: &str = "type";
const KEY_START_TYPE: &str = "start_type";
const KEY_MODEL: &str = "model";
const KEY_PROVIDER: &str = "provider";
const KEY_TOOL_NAME: &str = "tool_name";
const KEY_DECISION: &str = "decision";
const KEY_SOURCE: &str = "source";
/// Redacted by [`crate::redact_for_export`] as well as by the call sites here.
pub(crate) const KEY_ERROR: &str = "error";
pub(crate) const KEY_PROMPT: &str = "prompt";
pub(crate) const KEY_TOOL_INPUT: &str = "tool_input";

const MSG_USER_PROMPT: &str = "user prompt";
const MSG_API_REQUEST: &str = "api request";
const MSG_API_ERROR: &str = "api error";
const MSG_TOOL_RESULT: &str = "tool result";
const MSG_TOOL_DECISION: &str = "tool decision";
const MSG_DECISION: &str = "decision engine";

/// The one entry point for session starts: the id is set before counting, so
/// a counted session can never miss it.
pub fn session_started(start_type: &'static str, session_id: Option<&str>) {
    if let Some(id) = session_id {
        crate::set_session_id(id);
    }
    let Some(handle) = handle() else {
        return;
    };
    handle.record(
        &SESSION_COUNT,
        Value::Int(1),
        AttrSet::new().with(KEY_START_TYPE, start_type),
    );
}

/// Prompt text is opt-in, and the gate lives here so the text is never
/// formatted, never reaches the log file, and never reaches the exporter
/// unless it was asked for.
fn opt_in_prompt(prompt: &str) -> Option<String> {
    let handle = handle()?;
    handle.log_user_prompts.then(|| handle.truncate(prompt))
}

pub fn user_prompt(prompt: &str) {
    tracing::info!(
        target: EVENT_USER_PROMPT,
        prompt_length = prompt.chars().count(),
        prompt = opt_in_prompt(prompt),
        MSG_USER_PROMPT,
    );
}

pub struct ApiRequest<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
    /// What a subscription covered, at API list rates. Reported alongside
    /// `cost_usd` but never added to it, and never recorded as spend.
    pub subscription_cost_usd: f64,
    pub duration: Duration,
    pub stop_reason: Option<&'a str>,
}

pub fn api_request(request: &ApiRequest<'_>) {
    tracing::info!(
        target: EVENT_API_REQUEST,
        model = request.model,
        provider = request.provider,
        input_tokens = request.input_tokens,
        output_tokens = request.output_tokens,
        cache_read_tokens = request.cache_read_tokens,
        cache_creation_tokens = request.cache_creation_tokens,
        cost_usd = request.cost_usd,
        subscription_cost_usd = request.subscription_cost_usd,
        duration_ms = request.duration.as_millis() as u64,
        stop_reason = request.stop_reason,
        MSG_API_REQUEST,
    );

    let Some(handle) = handle() else {
        return;
    };
    let model_attrs = AttrSet::new()
        .with(KEY_MODEL, request.model)
        .with(KEY_PROVIDER, request.provider);
    for (kind, count) in [
        (TOKEN_INPUT, request.input_tokens),
        (TOKEN_OUTPUT, request.output_tokens),
        (TOKEN_CACHE_READ, request.cache_read_tokens),
        (TOKEN_CACHE_CREATION, request.cache_creation_tokens),
    ] {
        if count == 0 {
            continue;
        }
        handle.record(
            &TOKEN_USAGE,
            Value::Int(count as i64),
            model_attrs.clone().with(KEY_TYPE, kind),
        );
    }
    if request.cost_usd > 0.0 {
        handle.record(&COST_USAGE, Value::Double(request.cost_usd), model_attrs);
    }
}

pub struct ApiError<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub error: &'a str,
    pub status_code: Option<u16>,
    pub attempt: u32,
    pub duration: Duration,
}

pub fn api_error(error: &ApiError<'_>) {
    tracing::warn!(
        target: EVENT_API_ERROR,
        model = error.model,
        provider = error.provider,
        error = error.error,
        status_code = error.status_code,
        attempt = error.attempt,
        duration_ms = error.duration.as_millis() as u64,
        MSG_API_ERROR,
    );
}

pub struct ToolResult<'a> {
    pub tool_name: &'a str,
    pub tool_source: &'a str,
    pub success: bool,
    pub duration: Duration,
    pub error_type: Option<&'a str>,
    pub tool_input: Option<&'a str>,
}

/// Tool input is opt-in for the same reason prompts are: it routinely carries
/// file contents, shell commands, and paths.
fn opt_in_tool_input(input: Option<&str>) -> Option<String> {
    let handle = handle()?;
    handle
        .log_tool_details
        .then_some(input)
        .flatten()
        .map(|input| handle.truncate(input))
}

pub fn tool_result(result: &ToolResult<'_>) {
    tracing::info!(
        target: EVENT_TOOL_RESULT,
        tool_name = result.tool_name,
        tool_source = result.tool_source,
        success = result.success,
        duration_ms = result.duration.as_millis() as u64,
        error_type = result.error_type,
        tool_input = opt_in_tool_input(result.tool_input),
        MSG_TOOL_RESULT,
    );
}

/// Both the event and the counter: dashboards want the rate, audits the detail.
pub fn tool_decision(tool_name: &str, decision: &'static str, source: &'static str) {
    tracing::info!(
        target: EVENT_TOOL_DECISION,
        tool_name,
        decision,
        source,
        MSG_TOOL_DECISION,
    );
    if let Some(handle) = handle() {
        handle.record(
            &TOOL_DECISION,
            Value::Int(1),
            AttrSet::new()
                .with(KEY_TOOL_NAME, tool_name)
                .with(KEY_DECISION, decision)
                .with(KEY_SOURCE, source),
        );
    }
}

pub fn decision(
    feature: &'static str,
    effect: &'static str,
    latency_ms: u64,
    model: &str,
    error: Option<&'static str>,
) {
    tracing::info!(
        target: EVENT_DECISION,
        feature,
        effect,
        latency_ms,
        model,
        error,
        MSG_DECISION,
    );
}

pub fn lines_of_code(added: u64, removed: u64) {
    let Some(handle) = handle() else {
        return;
    };
    for (kind, count) in [(LINES_ADDED, added), (LINES_REMOVED, removed)] {
        if count == 0 {
            continue;
        }
        handle.record(
            &LINES_OF_CODE,
            Value::Int(count as i64),
            AttrSet::new().with(KEY_TYPE, kind),
        );
    }
}

pub fn commit_created() {
    if let Some(handle) = handle() {
        handle.record(&COMMIT_COUNT, Value::Int(1), AttrSet::new());
    }
}

pub fn pull_request_created() {
    if let Some(handle) = handle() {
        handle.record(&PULL_REQUEST_COUNT, Value::Int(1), AttrSet::new());
    }
}

/// Time the agent spent working, as opposed to waiting for the user.
pub fn active_time(duration: Duration) {
    if let Some(handle) = handle() {
        handle.record(
            &ACTIVE_TIME,
            Value::Double(duration.as_secs_f64()),
            AttrSet::new().with(KEY_TYPE, ACTIVE_TIME_CLI),
        );
    }
}
