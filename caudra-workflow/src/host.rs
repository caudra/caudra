use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal::CallKey;

/// Script spellings accepted for `capability_mode`: Grok Build's four plus Caudra's `build`.
pub const CAPABILITY_MODE_NAMES: [(&str, CapabilityMode); 5] = [
    ("read-only", CapabilityMode::ReadOnly),
    ("read-write", CapabilityMode::Build),
    ("execute", CapabilityMode::Build),
    ("all", CapabilityMode::Build),
    ("build", CapabilityMode::Build),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityMode {
    #[default]
    ReadOnly,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown capability_mode {0:?}; expected one of read-only, read-write, execute, all, build"
)]
pub struct UnknownCapabilityMode(pub String);

impl FromStr for CapabilityMode {
    type Err = UnknownCapabilityMode;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        CAPABILITY_MODE_NAMES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, mode)| *mode)
            .ok_or_else(|| UnknownCapabilityMode(name.to_owned()))
    }
}

/// One agent invocation as the script described it. Field order is the serialized order,
/// which the journal hashes after canonicalisation, so it is safe to extend at the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub prompt: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub capability_mode: CapabilityMode,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

impl AgentRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            label: None,
            capability_mode: CapabilityMode::default(),
            output_schema: None,
            phase: None,
            profile: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub agent_id: String,
    pub success: bool,
    pub output: Value,
    pub cancelled: bool,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

/// `Cancelled` and `BudgetExhausted` end the run; `Failed` and `Scratch` surface to the script as
/// catchable runtime errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("workflow cancelled")]
    Cancelled,
    #[error("agent budget exhausted")]
    BudgetExhausted,
    #[error("host failure: {0}")]
    Failed(String),
    #[error("scratch file failure: {0}")]
    Scratch(String),
}

/// Everything a workflow can ask of its environment. Results are committed durably by the host,
/// keyed by [`CallKey`]; the engine only asks and replays what the journal already holds.
pub trait WorkflowHost: Send + Sync {
    fn agent(&self, key: CallKey, request: &AgentRequest) -> Result<AgentResult, HostError>;

    /// Runs `requests` concurrently under keys `first_key..first_key + requests.len()` and returns
    /// results in request order.
    fn parallel(
        &self,
        first_key: CallKey,
        requests: &[AgentRequest],
    ) -> Result<Vec<AgentResult>, HostError>;

    fn phase(&self, title: &str);

    fn log(&self, message: &str);

    fn write_scratch_file(
        &self,
        key: CallKey,
        name: &str,
        content: &str,
    ) -> Result<String, HostError>;

    fn is_cancelled(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("read-only" => Ok(CapabilityMode::ReadOnly); "read_only")]
    #[test_case("read-write" => Ok(CapabilityMode::Build); "read_write")]
    #[test_case("execute" => Ok(CapabilityMode::Build); "execute")]
    #[test_case("all" => Ok(CapabilityMode::Build); "all")]
    #[test_case("build" => Ok(CapabilityMode::Build); "build")]
    #[test_case("readonly" => Err(UnknownCapabilityMode("readonly".into())); "unknown")]
    fn capability_mode_parsing(name: &str) -> Result<CapabilityMode, UnknownCapabilityMode> {
        name.parse()
    }

    #[test]
    fn agent_request_json_uses_kebab_case_mode_and_defaults() {
        let json = serde_json::to_value(AgentRequest {
            capability_mode: CapabilityMode::Build,
            ..AgentRequest::new("hi")
        })
        .expect("serializable");
        assert_eq!(json["capability_mode"], "build");
        let back: AgentRequest =
            serde_json::from_value(serde_json::json!({ "prompt": "hi" })).expect("defaults");
        assert_eq!(back, AgentRequest::new("hi"));
    }
}
