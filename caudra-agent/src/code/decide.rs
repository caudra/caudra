//! The `decisions` module: `decisions.decide()` and `decisions.rerank()` in a snippet, the
//! decision dialect workflows use, sent through the session's decision engine under the `code`
//! feature, so a snippet can judge and sort what it gathered before any of it reaches the context.

use caudra_decision::DecisionError;
use caudra_workflow::DecisionRequest;
use caudra_workflow::decision::{
    PROBABILITY, RERANK_CANDIDATES_TYPE, RERANK_QUERY_TYPE, RERANK_THRESHOLD_RANGE,
    RERANK_TOP_K_TYPE, RerankRequest, ScriptQuestions, decide_label, ranked, rerank_batches,
    rerank_label, rerank_scores,
};
use serde_json::{Map, Value, json};

use super::args::bind;
use super::gateway::{DECISIONS_ROW, Permission};
use super::module::{
    CallFailure, CallRun, HostCallPlan, HostModule, MemberDoc, ModuleDoc, Prepared,
};
use super::names::{DECIDE_HELPER, DECISIONS_MODULE, RERANK_HELPER};
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind, ScriptInvocation};
use crate::CancelToken;
use crate::decisions::{DecisionContext, DecisionFeature, Decisions, join_answered};
use crate::mcp::output::{DecisionHelper, McpBlock, McpHelper, McpOutput};
use crate::tools::{ToolContext, ToolFailure};

/// The guidance that makes decisions worth having, shown in full by `help("decisions")` and, with
/// an engine, in the `# MCP servers` reminder.
pub const GUIDANCE: &str = "Use these on what a snippet gathered, to keep only what matters before it reaches you or to branch on a judgement without another round trip. They change nothing, and their answers are probabilities.\n\
- `await decisions.decide(input, questions)`: answers every question about `input` in one request. Questions follow the OpenAI Decisions shape: `type`, `name`, `instructions`, as a `predicate`, a `choice` with `choices`, or a `score` with 2 to 10 `levels`, low to high. Read `r[\"by_name\"][name]`.\n\
- `await decisions.rerank(query, candidates, threshold=None, top_k=None)`: `[{\"index\", \"score\"}]`, best first, for up to 256 strings or small dicts. `score` is the probability of relevance.\n\
For good answers:\n\
- Make `input` a dict under 1,500 bytes of only the judged fields, and name them in the questions. `instructions` can be `{\"question\": ..., \"focus\": \"what counts and what does not\"}`.\n\
- Describe each choice or level by what sets it apart.\n\
- Write `query` as the need, not keywords, and pass short labels as candidates. `index` maps back to the full item.\n\
- Act on about 0.85 or more; below 0.6 is a guess.\n\
- `decide` judges `input` as one item: call it once per item. A local engine queues overlapping calls, and each waits only `timeout_ms` (800 by default), so raise it to `gather` many.\n\
Examples: `await decisions.decide({\"body\": b}, [{\"type\": \"score\", \"name\": \"severity\", \"instructions\": \"How severe is the bug `body` reports?\", \"levels\": [\"cosmetic\", {\"label\": \"blocking\", \"description\": \"no workaround\"}]}])`, `await decisions.rerank(\"issues reporting a crash at startup\", titles, top_k=10)`.";

pub static DOC: ModuleDoc = ModuleDoc {
    namespace: DECISIONS_MODULE,
    summary: "`decisions`: judge or sort what a snippet gathered with the session's decision engine, when one is set up: `decisions.decide`, `decisions.rerank`.",
    page: GUIDANCE,
    members: &[
        MemberDoc {
            name: DECIDE_HELPER,
            signature: "decide(input: Any, questions: list[dict[str, Any]], model: str | None = None, timeout_ms: int | None = None) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Returns `{\"answers\": [...], \"by_name\": {name: answer}, \"model\"}`: a predicate answers `probability`, a choice `choice` with `probabilities`, a score `score` with `probabilities`. It asks no permission, and works while planning.",
        },
        MemberDoc {
            name: RERANK_HELPER,
            signature: "rerank(query: str, candidates: list[Any], threshold: float | None = None, top_k: int | None = None, model: str | None = None, timeout_ms: int | None = None) -> list[dict[str, Any]]",
            awaited: true,
            read_first: false,
            page: "Returns `[{\"index\", \"score\"}]` best first, keeping only scores at or above `threshold` and at most `top_k`. Judges up to 256 strings or small dicts, 16 to a request. Without `timeout_ms` each request waits the engine's rerank budget.",
        },
    ],
};
const DECIDE_CALL: &str = "decisions.decide";
const RERANK_CALL: &str = "decisions.rerank";
const NO_ENGINE: &str =
    "decisions is unavailable: no decision engine is set up; choose one with /decision-model";
const DECIDE_PARAMS: &[&str] = &[INPUT_PARAM, QUESTIONS_PARAM, MODEL_PARAM, TIMEOUT_PARAM];
const RERANK_PARAMS: &[&str] = &[
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
    "decisions.rerank() judges at most 256 candidates per call; filter the list first or split it";
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

/// Reads `decisions.decide(input, questions, model=None, timeout_ms=None)`.
pub fn decide_request(arguments: &Map<String, Value>) -> Result<Decide, ScriptError> {
    let helper = DECIDE_CALL;
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

/// Reads `decisions.rerank(query, candidates, threshold=None, top_k=None, model=None, timeout_ms=None)`.
pub fn rerank_request(arguments: &Map<String, Value>) -> Result<RerankRequest, ScriptError> {
    let query = match arguments.get(QUERY_PARAM) {
        None => return Err(missing(RERANK_CALL, QUERY_PARAM)),
        Some(Value::String(query)) if !query.trim().is_empty() => query.clone(),
        Some(_) => return Err(value_error(RERANK_QUERY_TYPE)),
    };
    let candidates = match arguments.get(CANDIDATES_PARAM) {
        None => return Err(missing(RERANK_CALL, CANDIDATES_PARAM)),
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

/// The session's decision engine as a module, which exists in every run so `help()` can say why
/// it is unavailable.
pub struct DecisionsModule {
    decisions: Option<Decisions>,
}

impl DecisionsModule {
    pub fn new(decisions: Option<Decisions>) -> Self {
        Self { decisions }
    }
}

impl HostModule for DecisionsModule {
    fn doc(&self) -> &'static ModuleDoc {
        &DOC
    }

    fn unavailable(&self) -> Option<String> {
        self.decisions.is_none().then(|| NO_ENGINE.to_owned())
    }

    /// A call shown as a row like any other. It asks no permission: the engine is the user's own,
    /// and an answer is data that grants nothing.
    fn prepare(&self, _ctx: &ToolContext, call: ScriptCall) -> Prepared {
        let Some(decisions) = self.decisions.clone() else {
            return Prepared::Plan(Err(runtime_error(NO_ENGINE)));
        };
        match pending(&call) {
            Ok((Pending::Rerank(request), _)) if request.candidates.is_empty() => {
                Prepared::Answer(ScriptInvocation::Ready(Ok(Value::Array(Vec::new()))))
            }
            Ok((pending, arguments)) => {
                Prepared::Plan(Ok(Box::new(pending.plan(arguments, decisions))))
            }
            Err(error) => Prepared::Plan(Err(error)),
        }
    }
}

/// What a call asks, with the arguments its row keeps.
fn pending(call: &ScriptCall) -> Result<(Pending, Map<String, Value>), ScriptError> {
    match call.name.as_str() {
        RERANK_HELPER => {
            let arguments = bind(RERANK_CALL, RERANK_PARAMS, call)?;
            Ok((Pending::Rerank(rerank_request(&arguments)?), arguments))
        }
        DECIDE_HELPER => {
            let arguments = bind(DECIDE_CALL, DECIDE_PARAMS, call)?;
            Ok((Pending::Decide(decide_request(&arguments)?), arguments))
        }
        name => Err(ScriptError::new(
            ScriptErrorKind::Attribute,
            DOC.no_member(name),
        )),
    }
}

/// A decision call whose arguments were read.
enum Pending {
    Decide(Decide),
    Rerank(RerankRequest),
}

impl Pending {
    fn plan(self, arguments: Map<String, Value>, decisions: Decisions) -> HostCallPlan {
        let (helper, marked, called_as) = match &self {
            Self::Decide(decide) => (
                DECIDE_HELPER,
                McpHelper::Decision(DecisionHelper::Decide),
                decide_label(&decide.request.questions),
            ),
            Self::Rerank(request) => (
                RERANK_HELPER,
                McpHelper::Decision(DecisionHelper::Rerank),
                rerank_label(request.candidates.len(), 1, 1),
            ),
        };
        let row = CodeCall {
            server: DECISIONS_ROW.to_owned(),
            tool: helper.to_owned(),
            called_as,
            input: Value::Object(arguments),
            helper: Some(marked),
            ..CodeCall::default()
        };
        let run: CallRun = Box::new(move |ctx: ToolContext| {
            Box::pin(async move { self.answer(&decisions, &ctx).await })
        });
        HostCallPlan {
            row,
            permission: Permission::none(),
            read_only: true,
            run,
        }
    }

    async fn answer(
        &self,
        decisions: &Decisions,
        ctx: &ToolContext,
    ) -> Result<(McpOutput, Value), CallFailure> {
        let context = ctx.permissions.decision_context(json!({
            "tool_use_id": ctx.tool_use_id,
        }));
        let answered = match self {
            Self::Decide(decide) => ask(decisions, &context, &decide.request, &ctx.cancel)
                .await
                .and_then(|result| {
                    decide
                        .questions
                        .answers(&result)
                        .map_err(|message| ScriptError::new(ScriptErrorKind::Runtime, message))
                }),
            Self::Rerank(request) => rerank(decisions, &context, request, &ctx.cancel).await,
        };
        match answered {
            Ok(value) => {
                let output = McpOutput {
                    blocks: vec![McpBlock::Text {
                        text: serde_json::to_string_pretty(&value).unwrap_or_default(),
                    }],
                    ..McpOutput::default()
                };
                Ok((output, value))
            }
            Err(error) => {
                let failure = match ctx.cancel.is_cancelled() {
                    true => ToolFailure::Cancelled,
                    false => ToolFailure::Other,
                };
                Err(CallFailure::new(failure, error.message))
            }
        }
    }
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
