//! Event records. caudra emits events as OTLP logs carrying their payload in
//! attributes rather than a body.
//!
//! Event names double as `tracing` targets. A `tracing::info!(target:
//! EVENT_API_REQUEST, ...)` reaches both the log file and the OTLP exporter,
//! which is why [`crate::layer`] allow-lists exactly [`EVENT_NAMES`].

use tracing::Level;

use crate::attr::AttrSet;

pub const SEVERITY_INFO: i32 = 9;
pub const SEVERITY_TEXT_INFO: &str = "INFO";

pub const EVENT_USER_PROMPT: &str = "caudra.user_prompt";
pub const EVENT_API_REQUEST: &str = "caudra.api_request";
pub const EVENT_API_ERROR: &str = "caudra.api_error";
pub const EVENT_TOOL_RESULT: &str = "caudra.tool_result";
pub const EVENT_TOOL_DECISION: &str = "caudra.tool_decision";
pub const EVENT_DECISION: &str = "caudra.decision";

/// The complete set of targets the telemetry layer forwards. An event outside
/// this list reaches the log file only, so a new `tracing` call site can never
/// widen what leaves the machine by accident.
pub const EVENT_NAMES: &[&str] = &[
    EVENT_USER_PROMPT,
    EVENT_API_REQUEST,
    EVENT_API_ERROR,
    EVENT_TOOL_RESULT,
    EVENT_TOOL_DECISION,
    EVENT_DECISION,
];

/// Resolves a target back to its `&'static str` so a `LogRecord` can borrow it.
pub fn event_name(target: &str) -> Option<&'static str> {
    EVENT_NAMES.iter().copied().find(|name| *name == target)
}

/// OTLP severity numbers, from the logs data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Severity {
    pub number: i32,
    pub text: &'static str,
}

impl Severity {
    pub const INFO: Self = Self {
        number: SEVERITY_INFO,
        text: SEVERITY_TEXT_INFO,
    };

    pub fn of(level: Level) -> Self {
        match level {
            Level::TRACE => Self {
                number: 1,
                text: "TRACE",
            },
            Level::DEBUG => Self {
                number: 5,
                text: "DEBUG",
            },
            Level::INFO => Self::INFO,
            Level::WARN => Self {
                number: 13,
                text: "WARN",
            },
            Level::ERROR => Self {
                number: 17,
                text: "ERROR",
            },
        }
    }
}

impl Default for Severity {
    fn default() -> Self {
        Self::INFO
    }
}

pub struct LogRecord {
    pub time_unix_nano: u64,
    pub event_name: &'static str,
    pub severity: Severity,
    pub attrs: AttrSet,
}
