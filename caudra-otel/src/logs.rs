//! Event records. caudra emits events as OTLP logs, all at INFO, carrying their
//! payload in attributes rather than a body.

use crate::attr::AttrSet;

pub const SEVERITY_INFO: i32 = 9;
pub const SEVERITY_TEXT_INFO: &str = "INFO";

pub const EVENT_USER_PROMPT: &str = "caudra.user_prompt";
pub const EVENT_API_REQUEST: &str = "caudra.api_request";
pub const EVENT_API_ERROR: &str = "caudra.api_error";
pub const EVENT_TOOL_RESULT: &str = "caudra.tool_result";
pub const EVENT_TOOL_DECISION: &str = "caudra.tool_decision";

pub struct LogRecord {
    pub time_unix_nano: u64,
    pub event_name: &'static str,
    pub attrs: AttrSet,
}
