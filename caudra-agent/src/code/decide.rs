//! `decide()` and `rerank()` in a snippet: the decision dialect workflows use, sent through the
//! session's decision engine under the `code` feature, so a snippet can judge and sort what its
//! servers returned before any of it reaches the context.

use caudra_decision::DecisionError;
use caudra_workflow::DecisionRequest;
use caudra_workflow::decision::{
    PROBABILITY, RERANK_CANDIDATES_TYPE, RERANK_QUERY_TYPE, RERANK_THRESHOLD_RANGE,
    RERANK_TOP_K_TYPE, RerankRequest, ScriptQuestions, ranked, rerank_batches, rerank_scores,
};
use serde_json::{Map, Value, json};

use super::names::{DECIDE_HELPER, RERANK_HELPER};
use super::runner::{ScriptError, ScriptErrorKind};
use crate::CancelToken;
use crate::decisions::{DecisionContext, DecisionFeature, Decisions, join_answered};

/// The decision helpers' signatures, for the type checker.
pub const DECISION_STUBS: [&str; 2] = [
    "async def decide(input: Any, questions: list[dict[str, Any]], model: str | None = None, timeout_ms: int | None = None) -> dict[str, Any]: ...",
    "async def rerank(query: str, candidates: list[Any], threshold: float | None = None, top_k: int | None = None, model: str | None = None, timeout_ms: int | None = None) -> list[dict[str, Any]]: ...",
];
pub const DECIDE_PARAMS: &[&str] = &[INPUT_PARAM, QUESTIONS_PARAM, MODEL_PARAM, TIMEOUT_PARAM];
pub const RERANK_PARAMS: &[&str] = &[
    QUERY_PARAM,
    CANDIDATES_PARAM,
    THRESHOLD_PARAM,
    TOP_K_PARAM,
    MODEL_PARAM,
    TIMEOUT_PARAM,
];
/// The most candidates one `rerank()` call judges: 16 requests of 16.
pub const MAX_RERANK_CANDIDATES: usize = 256;
/// Batch requests one `rerank()` call keeps in flight at once.
const MAX_RERANK_INFLIGHT: usize = 4;
const INPUT_PARAM: &str = "input";
const QUESTIONS_PARAM: &str = "questions";
const MODEL_PARAM: &str = "model";
const TIMEOUT_PARAM: &str = "timeout_ms";
const QUERY_PARAM: &str = "query";
const CANDIDATES_PARAM: &str = "candidates";
const THRESHOLD_PARAM: &str = "threshold";
const TOP_K_PARAM: &str = "top_k";
const INDEX_FIELD: &str = "index";
const SCORE_FIELD: &str = "score";
const MODEL_TYPE: &str = "model must be a non-empty str";
const TIMEOUT_TYPE: &str = "timeout_ms must be a positive int";
const TOO_MANY_CANDIDATES: &str =
    "rerank() judges at most 256 candidates per call; filter the list first or split it";
const UNAVAILABLE: &str = "the decision engine could not be reached";
const TIMED_OUT: &str = "the decision engine did not answer in time";
const INVALID_ANSWER: &str = "the decision engine sent an invalid answer";
const REJECTED: &str = "the decision request was rejected";
const REFUSED: &str = "the decision engine refused to answer";
const CANCELLED: &str = "the run was cancelled";

/// A `decide()` call: the questions to read the answers back by, and the internal request.
pub struct Decide {
    pub questions: ScriptQuestions,
    pub request: DecisionRequest,
}

/// Reads `decide(input, questions, model=None, timeout_ms=None)`.
pub fn decide_request(arguments: &Map<String, Value>) -> Result<Decide, ScriptError> {
    let helper = DECIDE_HELPER;
    let input = arguments
        .get(INPUT_PARAM)
        .filter(|input| !input.is_null())
        .ok_or_else(|| missing(helper, INPUT_PARAM))?;
    let questions = arguments
        .get(QUESTIONS_PARAM)
        .ok_or_else(|| missing(helper, QUESTIONS_PARAM))?;
    let (questions, internal) = ScriptQuestions::parse(questions)
        .map_err(|message| ScriptError::new(ScriptErrorKind::Value, message))?;
    Ok(Decide {
        questions,
        request: DecisionRequest {
            state: input.clone(),
            questions: internal,
            model: model(arguments)?,
            timeout_ms: timeout(arguments)?,
        },
    })
}

/// Reads `rerank(query, candidates, threshold=None, top_k=None, model=None, timeout_ms=None)`.
pub fn rerank_request(arguments: &Map<String, Value>) -> Result<RerankRequest, ScriptError> {
    let query = match arguments.get(QUERY_PARAM) {
        None => return Err(missing(RERANK_HELPER, QUERY_PARAM)),
        Some(Value::String(query)) if !query.trim().is_empty() => query.clone(),
        Some(_) => return Err(value_error(RERANK_QUERY_TYPE)),
    };
    let candidates = match arguments.get(CANDIDATES_PARAM) {
        None => return Err(missing(RERANK_HELPER, CANDIDATES_PARAM)),
        Some(Value::Array(candidates))
            if candidates
                .iter()
                .all(|candidate| candidate.is_string() || candidate.is_object()) =>
        {
            candidates.clone()
        }
        Some(_) => return Err(type_error(RERANK_CANDIDATES_TYPE)),
    };
    if candidates.len() > MAX_RERANK_CANDIDATES {
        return Err(value_error(TOO_MANY_CANDIDATES));
    }
    let threshold = match arguments.get(THRESHOLD_PARAM) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|threshold| PROBABILITY.contains(threshold))
                .ok_or_else(|| value_error(RERANK_THRESHOLD_RANGE))?,
        ),
    };
    let top_k = match arguments.get(TOP_K_PARAM) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|top_k| usize::try_from(top_k).ok())
                .filter(|top_k| *top_k > 0)
                .ok_or_else(|| value_error(RERANK_TOP_K_TYPE))?,
        ),
    };
    Ok(RerankRequest {
        query,
        candidates,
        model: model(arguments)?,
        timeout_ms: timeout(arguments)?,
        threshold,
        top_k,
    })
}

/// Asks one internal request and returns its `{answers, model}`.
pub async fn ask(
    decisions: &Decisions,
    context: &DecisionContext,
    request: &DecisionRequest,
    cancel: &CancelToken,
) -> Result<Value, ScriptError> {
    let outcome = cancel
        .race(decisions.scripted(DecisionFeature::Code, request, context))
        .await
        .map_err(|_| runtime_error(CANCELLED))?;
    let response = outcome.result.map_err(failure)?;
    Ok(json!({ "answers": response.answers, "model": response.model }))
}

/// Judges every candidate, a few batches at a time, and returns `[{index, score}]` best first.
/// One batch without an answer fails the call. Without a timeout, each batch gets the rerank
/// budget the built-in reranking features use, since a batch of 16 outlasts a single decision.
pub async fn rerank(
    decisions: &Decisions,
    context: &DecisionContext,
    request: &RerankRequest,
    cancel: &CancelToken,
) -> Result<Value, ScriptError> {
    let request = RerankRequest {
        timeout_ms: request
            .timeout_ms
            .or(Some(decisions.config().rerank_timeout_ms)),
        ..request.clone()
    };
    let request = &request;
    let batches = rerank_batches(request);
    let mut scores = Vec::with_capacity(request.candidates.len());
    for window in batches.chunks(MAX_RERANK_INFLIGHT) {
        let answered = join_answered(
            window
                .iter()
                .map(|(batch, _)| ask(decisions, context, batch, cancel)),
        )
        .await?;
        for ((batch, _), result) in window.iter().zip(answered) {
            let len = batch.questions.as_object().map_or(0, Map::len);
            scores.extend(
                rerank_scores(&result, len, scores.len())
                    .map_err(|message| runtime_error(&message))?,
            );
        }
    }
    Ok(ranked(&scores, request.threshold, request.top_k)
        .into_iter()
        .map(|(index, score)| json!({ INDEX_FIELD: index, SCORE_FIELD: score }))
        .collect())
}

fn model(arguments: &Map<String, Value>) -> Result<Option<String>, ScriptError> {
    match arguments.get(MODEL_PARAM) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(model)) if !model.trim().is_empty() => Ok(Some(model.clone())),
        Some(_) => Err(value_error(MODEL_TYPE)),
    }
}

fn timeout(arguments: &Map<String, Value>) -> Result<Option<u64>, ScriptError> {
    match arguments.get(TIMEOUT_PARAM) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|timeout| *timeout > 0)
            .map(Some)
            .ok_or_else(|| value_error(TIMEOUT_TYPE)),
    }
}

fn failure(error: DecisionError) -> ScriptError {
    runtime_error(match error {
        DecisionError::Unreachable | DecisionError::Http { .. } => UNAVAILABLE,
        DecisionError::Timeout => TIMED_OUT,
        DecisionError::Invalid(_) => INVALID_ANSWER,
        DecisionError::Rejected(reason) => {
            return runtime_error(&format!("{REJECTED}: {reason}"));
        }
        DecisionError::Refused => REFUSED,
    })
}

fn missing(helper: &str, param: &str) -> ScriptError {
    type_error(&format!("{helper}() missing required argument: '{param}'"))
}

fn type_error(message: &str) -> ScriptError {
    ScriptError::new(ScriptErrorKind::Type, message.to_owned())
}

fn value_error(message: &str) -> ScriptError {
    ScriptError::new(ScriptErrorKind::Value, message.to_owned())
}

fn runtime_error(message: &str) -> ScriptError {
    ScriptError::new(ScriptErrorKind::Runtime, message.to_owned())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn rerank_error(value: Value) -> ScriptError {
        rerank_request(&arguments(value)).unwrap_err()
    }

    #[test]
    fn decide_sends_the_input_as_state_with_internal_questions() {
        let decide = decide_request(&arguments(json!({
            "input": {"title": "Crash on save"},
            "questions": [{"type": "predicate", "name": "bug", "instructions": "A bug?"}],
            "timeout_ms": 500,
        })))
        .unwrap();
        assert_eq!(
            decide.request,
            DecisionRequest {
                state: json!({"title": "Crash on save"}),
                questions: json!({"bug": {"type": "noul", "instructions": "A bug?"}}),
                model: None,
                timeout_ms: Some(500),
            }
        );
    }

    #[test_case(json!({"questions": []}), ScriptErrorKind::Type ; "missing_input")]
    #[test_case(json!({"input": "x", "questions": {"bug": {}}}), ScriptErrorKind::Value ; "internal_question_map")]
    #[test_case(json!({"input": "x", "questions": [{"type": "predicate", "name": "a", "instructions": "x"}], "model": ""}), ScriptErrorKind::Value ; "blank_model")]
    #[test_case(json!({"input": "x", "questions": [{"type": "predicate", "name": "a", "instructions": "x"}], "timeout_ms": 0}), ScriptErrorKind::Value ; "zero_timeout")]
    fn a_malformed_decide_raises_before_any_call(value: Value, kind: ScriptErrorKind) {
        let Err(error) = decide_request(&arguments(value)) else {
            panic!("expected an error");
        };
        assert_eq!(error.kind, kind);
    }

    #[test_case(json!({"query": " ", "candidates": []}), RERANK_QUERY_TYPE ; "blank_query")]
    #[test_case(json!({"query": "q", "candidates": [1]}), RERANK_CANDIDATES_TYPE ; "numeric_candidate")]
    #[test_case(json!({"query": "q", "candidates": [], "threshold": 2}), RERANK_THRESHOLD_RANGE ; "threshold_above_one")]
    #[test_case(json!({"query": "q", "candidates": [], "top_k": 0}), RERANK_TOP_K_TYPE ; "zero_top_k")]
    fn a_malformed_rerank_raises_before_any_call(value: Value, message: &str) {
        assert_eq!(rerank_error(value).message, message);
    }

    #[test]
    fn rerank_refuses_more_candidates_than_it_judges() {
        let candidates = vec![json!("c"); MAX_RERANK_CANDIDATES + 1];
        assert_eq!(
            rerank_error(json!({"query": "q", "candidates": candidates})).message,
            TOO_MANY_CANDIDATES
        );
    }
}
