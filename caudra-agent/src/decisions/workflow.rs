use caudra_decision::wire::{MAX_CHOICE_OPTIONS, MAX_QUESTIONS};
use caudra_decision::{DecisionError, DecisionRequest, QuestionSet};
use caudra_workflow::DecisionRequest as WorkflowRequest;
use serde_json::{Map, Value};

use super::state::MAX_STATE_BYTES;
use super::{
    DecisionContext, DecisionFeature, DecisionOutcome, DecisionState, Decisions, error_kind,
};

const QUESTION_SET_ID: &str = "workflow.v1";
const INVALID_REQUEST: &str = "workflow decision request is invalid";
const UNSAFE_QUESTIONS: &str =
    "workflow questions cannot be projected without changing answer semantics";
const UNSAFE_MODEL: &str =
    "workflow model must be a bounded identifier without credentials or control characters";

impl Decisions {
    pub async fn workflow(
        &self,
        request: &WorkflowRequest,
        context: &DecisionContext,
    ) -> DecisionOutcome {
        let prepared = if self.enabled(&DecisionFeature::Workflow) {
            self.prepare_workflow(request)
        } else {
            Err(DecisionError::Unreachable)
        };
        match prepared {
            Ok((request, questions, timeout_ms)) => {
                self.run(
                    DecisionFeature::Workflow,
                    &questions,
                    request,
                    context,
                    timeout_ms,
                )
                .await
            }
            Err(error) => {
                caudra_otel::emit::decision("workflow", "none", 0, "", Some(error_kind(&error)));
                DecisionOutcome {
                    result: Err(error),
                    latency_ms: 0,
                    receipt: None,
                }
            }
        }
    }

    fn prepare_workflow(
        &self,
        request: &WorkflowRequest,
    ) -> Result<(DecisionRequest, QuestionSet, u64), DecisionError> {
        if request.state.is_null() || request.timeout_ms == Some(0) {
            return Err(DecisionError::Rejected(INVALID_REQUEST));
        }
        let model = request.model.as_deref().unwrap_or(&self.config().model);
        if model.trim().is_empty() || stable_text(model).is_err() {
            return Err(DecisionError::Rejected(UNSAFE_MODEL));
        }
        let state = DecisionState::new(&request.state)
            .map_err(|_| DecisionError::Rejected(super::STATE_REJECTED))?;
        let questions = project_questions(&request.questions)?;
        let request_wire = DecisionRequest {
            model: model.into(),
            state: state.value().clone(),
            questions: questions.questions().clone(),
        };
        request_wire.validate()?;
        Ok((
            request_wire,
            questions,
            request
                .timeout_ms
                .unwrap_or(self.config().timeout_ms)
                .min(self.config().timeout_ms),
        ))
    }
}

fn project_questions(value: &Value) -> Result<QuestionSet, DecisionError> {
    let questions = value
        .as_object()
        .filter(|questions| !questions.is_empty() && questions.len() <= MAX_QUESTIONS)
        .ok_or(DecisionError::Rejected(INVALID_REQUEST))?;
    let mut projected = Map::new();
    for (id, question) in questions {
        stable_text(id)?;
        let definition = question
            .as_object()
            .ok_or(DecisionError::Rejected(INVALID_REQUEST))?;
        let kind = definition
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "noul" | "choice" | "score"))
            .ok_or(DecisionError::Rejected(INVALID_REQUEST))?;
        let mut fields = Map::new();
        for (key, value) in definition {
            let value = match key.as_str() {
                "type" => value.clone(),
                "instructions" => redact(value)?,
                "criteria" => project_criteria(kind, value)?,
                _ => return Err(DecisionError::Rejected(INVALID_REQUEST)),
            };
            fields.insert(key.clone(), value);
        }
        projected.insert(id.clone(), Value::Object(fields));
    }
    let questions = serde_json::from_value(Value::Object(projected))
        .map_err(|_| DecisionError::Rejected(INVALID_REQUEST))?;
    QuestionSet::new(QUESTION_SET_ID, questions)
}

fn project_criteria(kind: &str, value: &Value) -> Result<Value, DecisionError> {
    if kind != "score"
        && let Some(criteria) = value.as_object()
    {
        if criteria.len() > MAX_CHOICE_OPTIONS {
            return Err(DecisionError::Rejected(INVALID_REQUEST));
        }
        let mut projected = Map::new();
        for (id, description) in criteria {
            stable_text(id)?;
            projected.insert(id.clone(), redact(description)?);
        }
        Ok(Value::Object(projected))
    } else if kind == "choice" {
        Err(DecisionError::Rejected(INVALID_REQUEST))
    } else {
        preserve(value)
    }
}

fn stable_text(text: &str) -> Result<(), DecisionError> {
    if text.len() > MAX_STATE_BYTES || text.chars().any(char::is_control) {
        return Err(DecisionError::Rejected(UNSAFE_QUESTIONS));
    }
    preserve(&Value::String(text.into())).map(|_| ())
}

fn preserve(value: &Value) -> Result<Value, DecisionError> {
    let redacted = redact(value)?;
    if redacted != *value {
        return Err(DecisionError::Rejected(UNSAFE_QUESTIONS));
    }
    Ok(redacted)
}

fn redact(value: &Value) -> Result<Value, DecisionError> {
    DecisionState::new(value)
        .map(|state| state.value().clone())
        .map_err(|_| DecisionError::Rejected(UNSAFE_QUESTIONS))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use caudra_config::decisions::DecisionsConfig;
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_storage::decision_log::{DecisionLabel, DecisionLog};
    use caudra_storage::usage_ledger::UsageLedger;
    use caudra_storage::{StateDir, now_epoch};
    use caudra_workflow::DecisionRequest as WorkflowRequest;
    use futures_lite::future;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        DecisionContext, DecisionFeature, Decisions, INVALID_REQUEST, UNSAFE_MODEL,
        project_questions,
    };

    const SECRET: &str = "do-not-send-this-value";
    const BASE_URL: &str = "http://127.0.0.1:1";
    const MODEL: &str = "workflow-model";
    const OTHER_MODEL: &str = "other-workflow-model";
    const TIMEOUT_MS: u64 = 5_000;
    const INPUT_TOKENS: u64 = 11;
    const OUTPUT_TOKENS: u64 = 3;
    const WORKFLOW_NAME: &str = "deployment";

    struct FakeEngine {
        calls: Arc<Mutex<Vec<(DecisionRequest, Duration)>>>,
        pending: bool,
    }

    #[async_trait]
    impl DecisionEngine for FakeEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.lock().unwrap().push((
                request.clone(),
                deadline.saturating_duration_since(Instant::now()),
            ));
            if self.pending {
                return future::pending().await;
            }
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers: request
                    .questions
                    .keys()
                    .map(|id| (id.clone(), Answer::Noul(NoulAnswer { noul: 1.0 })))
                    .collect(),
                usage: Usage {
                    input_tokens: INPUT_TOKENS,
                    output_tokens: OUTPUT_TOKENS,
                },
                cache_hit: false,
            })
        }
    }

    fn config(log: bool) -> DecisionsConfig {
        DecisionsConfig {
            base_url: Some(BASE_URL.parse().unwrap()),
            timeout_ms: TIMEOUT_MS,
            log,
            ..Default::default()
        }
    }

    fn request() -> WorkflowRequest {
        WorkflowRequest {
            state: json!({"command": format!("TOKEN={SECRET} command")}),
            questions: json!({"credentials": {"type": "noul", "instructions": format!("Assess TOKEN={SECRET} command")}}),
            model: Some(MODEL.into()),
            timeout_ms: Some(TIMEOUT_MS * 2),
        }
    }

    #[test]
    fn passive_off_workflows_redact_log_and_count_uncached_usage_by_model() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state");
            let state_dir = StateDir::from_path(path.clone());
            let calls = Arc::new(Mutex::new(Vec::new()));
            let service = Decisions::with_engine(
                config(true),
                &state_dir,
                FakeEngine {
                    calls: calls.clone(),
                    pending: false,
                },
            )
            .unwrap();
            assert!(service.enabled(&DecisionFeature::Workflow));
            assert!(!service.enabled(&DecisionFeature::PermissionAdvice));
            assert!(!path.exists());
            let context = DecisionContext {
                meta: json!({"workflow_name": WORKFLOW_NAME, "api_key": SECRET}),
                ..Default::default()
            };
            let mut request = request();
            let first = service.workflow(&request, &context).await;
            assert!(!first.result.unwrap().cache_hit);
            service
                .attach_label(
                    &first.receipt.unwrap(),
                    &DecisionLabel {
                        expected: json!({"credentials": true}),
                        source: "user".into(),
                        timestamp: now_epoch(),
                        meta: Value::Null,
                    },
                )
                .await
                .unwrap();
            let cached = service.workflow(&request, &context).await.result.unwrap();
            assert!(cached.cache_hit);
            request.model = Some(OTHER_MODEL.into());
            assert!(
                !service
                    .workflow(&request, &context)
                    .await
                    .result
                    .unwrap()
                    .cache_hit
            );
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0.model, MODEL);
            assert_eq!(calls[1].0.model, OTHER_MODEL);
            assert!(
                calls
                    .iter()
                    .all(|(_, budget)| *budget <= Duration::from_millis(TIMEOUT_MS))
            );
            assert!(!serde_json::to_string(&calls[0].0).unwrap().contains(SECRET));
            assert!(calls[0].0.questions.contains_key("credentials"));
            let mut output = Vec::new();
            DecisionLog::open_existing(&state_dir)
                .unwrap()
                .unwrap()
                .export_jsonl(&mut output, Some("workflow"))
                .unwrap();
            let exported: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(exported["state"], calls[0].0.state);
            assert_eq!(
                exported["questions"],
                serde_json::to_value(&calls[0].0.questions).unwrap()
            );
            assert_eq!(exported["caudra"]["meta"]["workflow_name"], WORKFLOW_NAME);
            assert!(!String::from_utf8(output).unwrap().contains(SECRET));
            let usage = UsageLedger::open(&state_dir).unwrap().lifetime().unwrap();
            assert_eq!(usage.input, INPUT_TOKENS * 2);
            assert_eq!(usage.output, OUTPUT_TOKENS * 2);
            assert_eq!(usage.unpriced_turns, 2);
            assert_eq!(usage.priced_turns, 0);
            assert_eq!(usage.by_purpose[0].label, "decision");
        });
    }

    #[test_case(None, TIMEOUT_MS; "configured_timeout")]
    #[test_case(Some(TIMEOUT_MS * 2), TIMEOUT_MS; "larger_override_is_capped")]
    #[test_case(Some(TIMEOUT_MS / 2), TIMEOUT_MS / 2; "smaller_override_is_honored")]
    fn workflow_timeout_and_default_model_are_projected(timeout: Option<u64>, expected: u64) {
        let root = tempfile::tempdir().unwrap();
        let config = config(false);
        let model = config.model.clone();
        let service = Decisions::with_engine(
            config,
            &StateDir::from_path(root.path().into()),
            FakeEngine {
                calls: Arc::default(),
                pending: false,
            },
        )
        .unwrap();
        let mut request = request();
        request.model = None;
        request.timeout_ms = timeout;
        let (wire, _, timeout_ms) = service.prepare_workflow(&request).unwrap();
        assert_eq!(wire.model, model);
        assert_eq!(timeout_ms, expected);
    }

    #[test]
    fn ignoring_workflow_deadline_is_still_bounded() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let service = Decisions::with_engine(
                config(false),
                &StateDir::from_path(root.path().into()),
                FakeEngine {
                    calls: Arc::default(),
                    pending: true,
                },
            )
            .unwrap();
            let mut request = request();
            request.timeout_ms = Some(1);
            assert_eq!(
                service
                    .workflow(&request, &DecisionContext::default())
                    .await
                    .result
                    .unwrap_err(),
                DecisionError::Timeout
            );
        });
    }

    #[test]
    fn no_base_url_returns_unavailable_without_creating_state() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("absent");
            let service = Decisions::new(
                DecisionsConfig::default(),
                &StateDir::from_path(path.clone()),
            )
            .unwrap();
            assert_eq!(
                service
                    .workflow(&request(), &DecisionContext::default())
                    .await
                    .result
                    .unwrap_err(),
                DecisionError::Unreachable
            );
            assert!(!path.exists());
        });
    }

    #[test]
    fn unsafe_model_is_rejected_without_engine_calls() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let service = Decisions::with_engine(
                config(false),
                &StateDir::from_path(root.path().into()),
                FakeEngine {
                    calls: calls.clone(),
                    pending: false,
                },
            )
            .unwrap();
            let mut request = request();
            request.model = Some(format!("TOKEN={SECRET}"));
            let error = service
                .workflow(&request, &DecisionContext::default())
                .await
                .result
                .unwrap_err();
            assert_eq!(error, DecisionError::Rejected(UNSAFE_MODEL));
            assert!(!error.to_string().contains(SECRET));
            assert!(calls.lock().unwrap().is_empty());
        });
    }

    #[test_case(json!({"TOKEN=secret": {"type": "noul", "instructions": "test"}}); "question_id")]
    #[test_case(json!({"route": {"type": "choice", "instructions": "test", "criteria": {"TOKEN=secret": "description"}}}); "choice_option")]
    #[test_case(json!({"score": {"type": "score", "instructions": "test", "criteria": ["TOKEN=secret", "low"]}}); "score_legend")]
    fn redaction_never_renames_answers(questions: Value) {
        assert!(project_questions(&questions).is_err());
    }

    #[test_case(json!({"route": {"type": "choice", "instructions": "test", "criteria": ["fast", "slow"]}}); "choice_array")]
    #[test_case(json!({"route": {"type": "noul", "instructions": "test", "labels": ["yes", "no"]}}); "labels")]
    fn questions_follow_the_official_schema(questions: Value) {
        assert_eq!(
            project_questions(&questions).unwrap_err(),
            DecisionError::Rejected(INVALID_REQUEST)
        );
    }

    #[test]
    fn choice_descriptions_redact_without_changing_ids_or_content_versions() {
        let project = |secret| {
            project_questions(&json!({"route": {
            "type": "choice", "instructions": "Choose", "criteria": {"credentials": format!("TOKEN={secret} description"), "other": "Nothing"}
        }})).unwrap()
        };
        let first = project(SECRET);
        let second = project("other-secret");
        assert_eq!(first.version(), second.version());
        assert!(
            first.questions()["route"]
                .criteria
                .as_ref()
                .unwrap()
                .get("credentials")
                .is_some()
        );
        assert!(
            !serde_json::to_string(first.questions())
                .unwrap()
                .contains(SECRET)
        );
    }
}
