use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal::CallKey;

pub const MAX_DECISION_QUESTIONS: usize = 64;
pub const MAX_DECISION_CHOICE_OPTIONS: usize = 100;
pub const MAX_DECISION_SCORE_LEVELS: usize = 10;
pub const MAX_DECISION_TOTAL_OPTIONS: usize = 512;
pub const MAX_DECISION_STATE_CHARS: usize = 50_000;
pub const MAX_DECISION_REQUEST_BYTES: usize = 2 * 1024 * 1024;

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

/// Which of Caudra's model jobs runs an agent. A script names a job and never a
/// `provider/model-id`, so a workflow shared between machines asks for cheap
/// breadth or strong judgement without pinning a model nobody else has.
pub const MODEL_JOB_NAMES: [(&str, ModelJob); 5] = [
    ("chat", ModelJob::Chat),
    ("plan", ModelJob::Plan),
    ("subagent", ModelJob::Subagent),
    ("fast", ModelJob::Fast),
    ("best", ModelJob::Best),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelJob {
    Chat,
    Plan,
    Subagent,
    Fast,
    Best,
}

impl FromStr for ModelJob {
    type Err = UnknownModelJob;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        MODEL_JOB_NAMES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, job)| *job)
            .ok_or_else(|| UnknownModelJob(name.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown model_job {0:?}; expected one of chat, plan, subagent, fast, best")]
pub struct UnknownModelJob(pub String);

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
    /// Skipped when unset so a journal written before this field existed hashes
    /// to the same request and its run still resumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_job: Option<ModelJob>,
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
            model_job: None,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub state: Value,
    pub questions: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl DecisionRequest {
    pub fn validate(&self) -> Result<(), HostError> {
        let invalid =
            |message: &str| HostError::Failed(format!("invalid decision request: {message}"));
        if self.state.is_null() {
            return Err(invalid("state is required"));
        }
        let state_chars = match &self.state {
            Value::String(state) => state.chars().count(),
            state => state.to_string().chars().count(),
        };
        if state_chars > MAX_DECISION_STATE_CHARS {
            return Err(invalid("state exceeds the character limit"));
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(invalid("model must be a non-empty string"));
        }
        if self.timeout_ms == Some(0) {
            return Err(invalid("timeout_ms must be a positive integer"));
        }
        let questions = self
            .questions
            .as_object()
            .ok_or_else(|| invalid("questions must be a map"))?;
        if questions.is_empty() || questions.len() > MAX_DECISION_QUESTIONS {
            return Err(invalid("questions must contain 1 to 64 entries"));
        }
        let mut total_options = 0;
        for (id, question) in questions {
            let invalid = |message: &str| invalid(&format!("question `{id}`: {message}"));
            if id.trim().is_empty() {
                return Err(invalid("id must not be empty"));
            }
            let question = question
                .as_object()
                .ok_or_else(|| invalid("definition must be a map"))?;
            if question.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "instructions" | "criteria" | "labels"
                )
            }) {
                return Err(invalid(
                    "unknown field; expected type, instructions, criteria, or labels",
                ));
            }
            if !matches!(
                question.get("instructions"),
                Some(Value::String(_) | Value::Object(_) | Value::Array(_))
            ) {
                return Err(invalid("instructions must be a string, map, or array"));
            }
            let kind = question.get("type").and_then(Value::as_str);
            let criteria = question.get("criteria");
            let count = match kind {
                Some("choice") => {
                    let count = match criteria {
                        Some(Value::Object(options)) => options.len(),
                        Some(Value::Array(options))
                            if options.iter().all(|v| !v.is_array() && !v.is_object()) =>
                        {
                            let keys: BTreeSet<String> = options
                                .iter()
                                .map(|option| {
                                    option
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| option.to_string())
                                })
                                .collect();
                            if keys.len() != options.len() {
                                return Err(invalid("choice labels must be unique"));
                            }
                            options.len()
                        }
                        _ => {
                            return Err(invalid(
                                "choice criteria must be a map or array of scalar labels",
                            ));
                        }
                    };
                    if count == 0 || count > MAX_DECISION_CHOICE_OPTIONS {
                        return Err(invalid("choice criteria must contain 1 to 100 options"));
                    }
                    count
                }
                Some("score") => {
                    let Some(Value::Array(levels)) = criteria else {
                        return Err(invalid("score criteria must be an array"));
                    };
                    if levels.is_empty()
                        || levels.len() > MAX_DECISION_SCORE_LEVELS
                        || levels.iter().any(Value::is_null)
                    {
                        return Err(invalid(
                            "score criteria must contain 1 to 10 non-null levels",
                        ));
                    }
                    levels.len()
                }
                Some("noul") => {
                    match criteria {
                        None | Some(Value::Null) => (),
                        Some(Value::Object(criteria))
                            if criteria.keys().all(|key| {
                                key.eq_ignore_ascii_case("true")
                                    || key.eq_ignore_ascii_case("false")
                            }) => {}
                        _ => {
                            return Err(invalid(
                                "noul criteria must be a map keyed only true/false",
                            ));
                        }
                    }
                    0
                }
                _ => return Err(invalid("type must be noul, choice, or score")),
            };
            if question.contains_key("labels") && kind != Some("noul") {
                return Err(invalid("labels are only supported for noul questions"));
            }
            if let Some(labels) = question.get("labels").filter(|labels| !labels.is_null()) {
                let labels = labels
                    .as_object()
                    .ok_or_else(|| invalid("labels must be a map"))?;
                let false_label = labels.get("false").and_then(Value::as_str).map(str::trim);
                let true_label = labels.get("true").and_then(Value::as_str).map(str::trim);
                if labels.len() != 2
                    || false_label.is_none_or(str::is_empty)
                    || true_label.is_none_or(str::is_empty)
                    || false_label == true_label
                {
                    return Err(invalid(
                        "labels must map true/false to distinct non-empty strings for a noul",
                    ));
                }
            }
            total_options += count;
        }
        if total_options > MAX_DECISION_TOTAL_OPTIONS {
            return Err(invalid("questions exceed the total option limit"));
        }
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(&error.to_string()))?;
        if bytes.len() > MAX_DECISION_REQUEST_BYTES {
            return Err(invalid("request exceeds the byte limit"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionResult {
    pub answers: Value,
    pub model: String,
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

    fn decide(&self, key: CallKey, request: &DecisionRequest) -> Result<DecisionResult, HostError>;

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
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const UNSET_JOB_IS_ABSENT: &str =
        "an unset model_job must not appear in the request JSON at all";
    const INVALID_DECISION: &str = "invalid decision request";

    fn decision_request(questions: Value) -> DecisionRequest {
        DecisionRequest {
            state: json!("state"),
            questions,
            model: None,
            timeout_ms: None,
        }
    }

    #[test_case(json!({ "type": "noul", "instructions": "Ready?" }); "noul")]
    #[test_case(json!({ "type": "noul", "instructions": ["Ready?"], "criteria": { "true": ["yes"], "false": {"answer": "no"} }, "labels": { "true": "yes", "false": "no" } }); "structured_noul")]
    #[test_case(json!({ "type": "choice", "instructions": { "question": "Route?" }, "criteria": { "a": "first", "b": ["second"] } }); "choice_map")]
    #[test_case(json!({ "type": "choice", "instructions": "Route?", "criteria": ["a", 1, false, null] }); "choice_scalar_labels")]
    #[test_case(json!({ "type": "score", "instructions": "Level?", "criteria": ["low", { "description": "high" }] }); "score")]
    fn valid_decision_question_shapes(question: Value) {
        decision_request(json!({ "q": question }))
            .validate()
            .unwrap();
    }

    #[test_case(json!([]); "questions_array")]
    #[test_case(json!({}); "questions_empty")]
    #[test_case(json!({ "q": false }); "definition_boolean")]
    #[test_case(json!({ "q": { "type": "bool", "instructions": "Ready?" } }); "unsupported_type")]
    #[test_case(json!({ "q": { "type": "noul" } }); "missing_instructions")]
    #[test_case(json!({ "q": { "type": "noul", "instructions": 1 } }); "numeric_instructions")]
    #[test_case(json!({ "q": { "type": "noul", "instructions": "Ready?", "criteria": [] } }); "noul_array")]
    #[test_case(json!({ "q": { "type": "noul", "instructions": "Ready?", "criteria": { "yes": "yes" } } }); "noul_wrong_keys")]
    #[test_case(json!({ "q": { "type": "noul", "instructions": "Ready?", "labels": { "false": "same", "true": "same" } } }); "noul_duplicate_labels")]
    #[test_case(json!({ "q": { "type": "choice", "instructions": "Route?", "criteria": {} } }); "empty_choice")]
    #[test_case(json!({ "q": { "type": "choice", "instructions": "Route?", "criteria": [["nested"]] } }); "choice_nested_label")]
    #[test_case(json!({ "q": { "type": "choice", "instructions": "Route?", "criteria": ["same", "same"] } }); "choice_duplicate_label")]
    #[test_case(json!({ "q": { "type": "noul", "instructions": "Ready?", "typo": "x" } }); "unknown_field")]
    #[test_case(json!({ "": { "type": "noul", "instructions": "Ready?" } }); "empty_id")]
    #[test_case(json!({ "q": { "type": "score", "instructions": "Level?", "criteria": { "0": "low" } } }); "score_map")]
    #[test_case(json!({ "q": { "type": "score", "instructions": "Level?", "criteria": [null] } }); "score_null_level")]
    #[test_case(json!({ "q": { "type": "score", "instructions": "Level?", "criteria": ["low"], "labels": null } }); "score_labels")]
    fn invalid_decision_question_shapes(questions: Value) {
        let error = decision_request(questions).validate().unwrap_err();
        assert!(error.to_string().contains(INVALID_DECISION));
    }

    #[test_case("choice", MAX_DECISION_CHOICE_OPTIONS; "choice")]
    #[test_case("score", MAX_DECISION_SCORE_LEVELS; "score")]
    fn decision_option_limits(kind: &str, limit: usize) {
        let mut request = decision_request(json!({ "q": {
            "type": kind, "instructions": "Choose", "criteria": (0..limit).map(|index| index.to_string()).collect::<Vec<_>>(),
        }}));
        request.validate().unwrap();
        request.questions["q"]["criteria"]
            .as_array_mut()
            .unwrap()
            .push(json!("extra"));
        assert!(request.validate().is_err());
    }

    #[test]
    fn decision_question_count_and_total_options_are_bounded() {
        let mut request = decision_request(Value::Object(
            (0..MAX_DECISION_QUESTIONS)
                .map(|index| {
                    (
                        index.to_string(),
                        json!({ "type": "noul", "instructions": "Ready?" }),
                    )
                })
                .collect(),
        ));
        request.validate().unwrap();
        request.questions["extra"] = json!({ "type": "noul", "instructions": "Ready?" });
        assert!(request.validate().is_err());
        request.questions = Value::Object((0..MAX_DECISION_TOTAL_OPTIONS.div_ceil(MAX_DECISION_CHOICE_OPTIONS)).map(|index| {
            (index.to_string(), json!({ "type": "choice", "instructions": "Route?", "criteria": (0..MAX_DECISION_CHOICE_OPTIONS).map(|index| index.to_string()).collect::<Vec<_>>() }))
        }).collect());
        assert!(request.validate().is_err());
    }

    #[test]
    fn decision_state_and_body_limits() {
        let mut request =
            decision_request(json!({ "q": { "type": "noul", "instructions": "Ready?" } }));
        request.state = json!("é".repeat(MAX_DECISION_STATE_CHARS));
        request.validate().unwrap();
        request.state = json!("é".repeat(MAX_DECISION_STATE_CHARS + 1));
        assert!(request.validate().is_err());
        request.state = Value::Null;
        assert!(request.validate().is_err());
        request.state = json!("small");
        request.questions["q"]["instructions"] = json!("x".repeat(MAX_DECISION_REQUEST_BYTES));
        assert!(request.validate().is_err());
    }

    #[test_case("read-only" => Ok(CapabilityMode::ReadOnly); "read_only")]
    #[test_case("read-write" => Ok(CapabilityMode::Build); "read_write")]
    #[test_case("execute" => Ok(CapabilityMode::Build); "execute")]
    #[test_case("all" => Ok(CapabilityMode::Build); "all")]
    #[test_case("build" => Ok(CapabilityMode::Build); "build")]
    #[test_case("readonly" => Err(UnknownCapabilityMode("readonly".into())); "unknown")]
    fn capability_mode_parsing(name: &str) -> Result<CapabilityMode, UnknownCapabilityMode> {
        name.parse()
    }

    #[test_case("fast" => Ok(ModelJob::Fast); "fast")]
    #[test_case("best" => Ok(ModelJob::Best); "best")]
    #[test_case("subagent" => Ok(ModelJob::Subagent); "subagent")]
    #[test_case("opus" => Err(UnknownModelJob("opus".into())); "a model is not a job")]
    fn model_job_parsing(name: &str) -> Result<ModelJob, UnknownModelJob> {
        name.parse()
    }

    /// The journal hashes this JSON, so a field that serialized as `null` when
    /// unset would change every request written before it existed and no run
    /// started by an older build could resume.
    #[test]
    fn an_unset_model_job_leaves_the_request_json_untouched() {
        let json = serde_json::to_value(AgentRequest::new("hi")).expect("serializable");

        assert_eq!(json.get("model_job"), None, "{UNSET_JOB_IS_ABSENT}");
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
