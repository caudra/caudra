use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::engine::{EngineError, RhaiEngine, RunParams, WorkflowEngine};
use crate::host::{
    AgentRequest, AgentResult, DecisionRequest, DecisionResult, HostError, WorkflowHost,
};
use crate::journal::{CallKey, Journal};
use crate::meta::{MetaError, WorkflowMeta, parse_meta};
use crate::run::{EngineLimits, WorkflowOutcome};

const SMOKE_QUERY: &str = "smoke query";
const SMOKE_OBJECTIVE: &str = "smoke objective";
const SMOKE_AGENT_ID: &str = "smoke";
const SMOKE_DECISION_MODEL: &str = "smoke";
const SMOKE_SCRATCH_DIR: &str = "smoke-scratch";
const SMOKE_MAX_OPERATIONS: u64 = 5_000_000;
const SMOKE_MAX_HOST_CALLS: u64 = 64;
/// Enough that a budget-aware script takes its full path during validation
/// rather than the degraded one it keeps for a nearly spent run.
const SMOKE_AGENT_BUDGET: u32 = 128;
const SMOKE_WALL_TIME: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    pub meta: WorkflowMeta,
    pub smoke: SmokeResult,
}

/// What one canned run observed. Agents answer `{}` and scratch writes succeed, so this exercises
/// exactly one path through the script; a `Failed` outcome is reported as `ValidationError::Smoke`.
#[derive(Debug, Clone, PartialEq)]
pub struct SmokeResult {
    pub outcome: WorkflowOutcome,
    pub host_calls: u64,
    pub phases_seen: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("meta: {0}")]
    Meta(#[from] MetaError),
    #[error(transparent)]
    Compile(#[from] EngineError),
    #[error("smoke run failed: {0}")]
    Smoke(String),
}

#[derive(Default)]
struct SmokeHost {
    calls: AtomicU64,
    phases: Mutex<Vec<String>>,
}

impl SmokeHost {
    fn inert_result(&self) -> AgentResult {
        self.calls.fetch_add(1, Ordering::Relaxed);
        AgentResult {
            agent_id: SMOKE_AGENT_ID.into(),
            success: true,
            output: json!({}),
            cancelled: false,
            tokens_used: 0,
            duration_ms: 0,
        }
    }
}

impl WorkflowHost for SmokeHost {
    fn agent(&self, _key: CallKey, _request: &AgentRequest) -> Result<AgentResult, HostError> {
        Ok(self.inert_result())
    }

    fn decide(
        &self,
        _key: CallKey,
        request: &DecisionRequest,
    ) -> Result<DecisionResult, HostError> {
        request.validate()?;
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut answers = Map::new();
        if let Some(questions) = request.questions.as_object() {
            for (id, question) in questions {
                let answer = match question["type"].as_str() {
                    Some("choice") => {
                        let options: Vec<Value> = match &question["criteria"] {
                            Value::Object(options) => {
                                options.keys().cloned().map(Value::String).collect()
                            }
                            Value::Array(options) => options.clone(),
                            _ => Vec::new(),
                        };
                        let probabilities: Map<String, Value> = options
                            .iter()
                            .enumerate()
                            .map(|(index, option)| {
                                (
                                    option
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| option.to_string()),
                                    json!(if index == 0 { 1.0 } else { 0.0 }),
                                )
                            })
                            .collect();
                        json!({ "type": "choice", "choice": options.first(), "probabilities": probabilities, "confidence": 1.0 })
                    }
                    Some("score") => {
                        let mut legend = Map::new();
                        let mut probabilities = Map::new();
                        if let Some(levels) = question["criteria"].as_array() {
                            for (index, level) in levels.iter().enumerate() {
                                legend.insert(index.to_string(), level.clone());
                                probabilities.insert(
                                    index.to_string(),
                                    json!(if index == 0 { 1.0 } else { 0.0 }),
                                );
                            }
                        }
                        json!({ "type": "score", "score": 0.0, "legend": legend, "probabilities": probabilities, "confidence": 1.0 })
                    }
                    _ => json!({ "type": "noul", "noul": 0.0, "confidence": 1.0 }),
                };
                answers.insert(id.clone(), answer);
            }
        }
        Ok(DecisionResult {
            answers: Value::Object(answers),
            model: request
                .model
                .clone()
                .unwrap_or_else(|| SMOKE_DECISION_MODEL.into()),
        })
    }

    fn parallel(
        &self,
        _first_key: CallKey,
        requests: &[AgentRequest],
    ) -> Result<Vec<AgentResult>, HostError> {
        Ok(requests.iter().map(|_| self.inert_result()).collect())
    }

    fn phase(&self, title: &str) {
        self.phases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(title.to_owned());
    }

    fn log(&self, _message: &str) {}

    fn write_scratch_file(
        &self,
        _key: CallKey,
        name: &str,
        _content: &str,
    ) -> Result<String, HostError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(format!("{SMOKE_SCRATCH_DIR}/{name}"))
    }

    fn is_cancelled(&self) -> bool {
        false
    }
}

fn smoke_args() -> Value {
    json!({ "query": SMOKE_QUERY, "objective": SMOKE_OBJECTIVE })
}

/// Parses the header, compiles, then smoke-runs the script once against a canned host.
pub fn validate(source: &str) -> Result<ValidationReport, ValidationError> {
    let meta = parse_meta(source)?;
    RhaiEngine.compile(source)?;
    let host = SmokeHost::default();
    let limits = EngineLimits {
        max_operations: SMOKE_MAX_OPERATIONS,
        max_host_calls: SMOKE_MAX_HOST_CALLS,
        wall_time: SMOKE_WALL_TIME,
        ..EngineLimits::default()
    };
    let outcome = RhaiEngine.run(RunParams {
        source,
        args: &smoke_args(),
        journal: &Journal::new(),
        host: &host,
        limits: &limits,
        agent_budget: SMOKE_AGENT_BUDGET,
    });
    if let WorkflowOutcome::Failed(error) = outcome {
        return Err(ValidationError::Smoke(error));
    }
    Ok(ValidationReport {
        meta,
        smoke: SmokeResult {
            outcome,
            host_calls: host.calls.load(Ordering::Relaxed),
            phases_seen: host
                .phases
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DEEP_RESEARCH_NAME, DEEP_RESEARCH_SOURCE, REVIEW_CHANGES_NAME, REVIEW_CHANGES_SOURCE,
        ROOT_CAUSE_NAME, ROOT_CAUSE_SOURCE,
    };

    const META: &str = r#"let meta = #{ name: "t", description: "d" };"#;

    #[test]
    fn decisions_smoke_run_with_deterministic_typed_answers() {
        let source = format!(
            r#"{META}
            let result = decide(args, #{{
                ready: #{{ type: "noul", instructions: "Ready?" }},
                route: #{{ type: "choice", instructions: "Route?", criteria: #{{ a: "first", b: "second" }} }},
                level: #{{ type: "score", instructions: "Level?", criteria: ["low", "high"] }}
            }});
            complete([result.answers.ready.noul, result.answers.route.choice, result.answers.level.score, result.model]);"#
        );
        let report = validate(&source).unwrap();
        assert_eq!(report.smoke.host_calls, 1);
        assert_eq!(
            report.smoke.outcome,
            WorkflowOutcome::Completed(json!([0.0, "a", 0.0, SMOKE_DECISION_MODEL]))
        );
        assert_eq!(
            validate(&source).unwrap().smoke.outcome,
            report.smoke.outcome
        );
    }

    #[test]
    fn deep_research_validates_on_the_inert_path() {
        let report = validate(DEEP_RESEARCH_SOURCE).expect("deep-research validates");
        assert_eq!(report.meta.name, DEEP_RESEARCH_NAME);
        assert_eq!(report.smoke.phases_seen, ["Plan", "Research"]);
        assert!(report.smoke.host_calls > 0);
        assert!(matches!(
            report.smoke.outcome,
            WorkflowOutcome::Completed(_)
        ));
    }

    /// Agents answer `{}` on the smoke path, so `readable` reads as false and
    /// this pins the earliest exit: a survey that read nothing still reaches a
    /// written artifact instead of dispatching reviewers over it.
    #[test]
    fn review_changes_validates_on_the_inert_path() {
        let report = validate(REVIEW_CHANGES_SOURCE).expect("review-changes validates");
        assert_eq!(report.meta.name, REVIEW_CHANGES_NAME);
        assert_eq!(report.smoke.phases_seen, ["Survey"]);
        assert!(matches!(
            report.smoke.outcome,
            WorkflowOutcome::Completed(_)
        ));
    }

    /// An inert host yields no observations, so this pins the earliest exit:
    /// a run that saw nothing still writes an artifact rather than failing.
    #[test]
    fn root_cause_validates_on_the_inert_path() {
        let report = validate(ROOT_CAUSE_SOURCE).expect("root-cause validates");
        assert_eq!(report.meta.name, ROOT_CAUSE_NAME);
        assert_eq!(report.smoke.phases_seen, ["Evidence"]);
        assert!(matches!(
            report.smoke.outcome,
            WorkflowOutcome::Completed(_)
        ));
    }

    #[test]
    fn runtime_error_on_the_smoke_path_is_reported() {
        let source = format!("{META}\nlet r = agent(\"go\");\nlet n = r.output.missing.deeper;\n");
        match validate(&source) {
            Err(ValidationError::Smoke(message)) => {
                assert!(message.contains("deeper"), "{message}")
            }
            other => panic!("expected smoke failure, got {other:?}"),
        }
    }

    #[test]
    fn meta_errors_take_precedence() {
        assert_eq!(
            validate("let x = 1;"),
            Err(ValidationError::Meta(MetaError::MetaNotFirst))
        );
    }

    #[test]
    fn paused_smoke_runs_are_reported_not_failed() {
        let source = format!("{META}\npause(\"user\", \"need input\");\n");
        let report = validate(&source).expect("pause is a valid outcome");
        assert!(matches!(
            report.smoke.outcome,
            WorkflowOutcome::Paused { .. }
        ));
        assert_eq!(report.smoke.host_calls, 0);
    }
}
