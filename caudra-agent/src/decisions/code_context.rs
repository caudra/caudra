use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, Decisions, MAX_RERANK_BATCH, Relevance, Rerank,
    candidate_text, redacted_excerpt,
};
use crate::CancelToken;
use crate::permissions::PermissionManager;

const MIN_RERANKED_ROWS: usize = 2;
const MAX_TASK_BYTES: usize = 700;
const MAX_NAME_BYTES: usize = 128;
const MAX_KIND_BYTES: usize = 32;
const MAX_PATH_BYTES: usize = 256;
const TASK_KEY: &str = "task";
const SYMBOL_KEY: &str = "symbol";
const KIND_KEY: &str = "kind";
const PATH_KEY: &str = "path";
const LINES_KEY: &str = "lines";
const CALLERS_KEY: &str = "callers";
const CALLEES_KEY: &str = "callees";
const RELEVANCE: Relevance = Relevance {
    statement: "The code symbol is worth reading before making the change the task describes.",
    relevant: "Its name, kind or path shows it implements, configures or tests what the task is about.",
    unrelated: "It only shares words with the task, or sits in an unrelated part of the code.",
};

/// One `code_context` row as the decision engine sees it: where the symbol
/// is and how connected it is, never its source.
pub struct CodeSymbol<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub path: &'a str,
    pub line_start: usize,
    pub line_end: usize,
    pub callers: usize,
    pub calls: usize,
}

/// Reorders the first [`MAX_RERANK_BATCH`] rows by relevance to `task`: the
/// rows at or above the relevance floor first, most relevant first, then the
/// rest of that head in its original order. Nothing is dropped and later rows
/// never move. Returns the note for the model when the order changed.
pub async fn rerank_code_context<T>(
    rows: &mut Vec<T>,
    describe: impl Fn(&T) -> CodeSymbol<'_>,
    task: &str,
    decisions: &Decisions,
    context: &DecisionContext,
    permissions: Option<&PermissionManager>,
    cancel: &CancelToken,
) -> Option<String> {
    let head = rows.len().min(MAX_RERANK_BATCH);
    if head < MIN_RERANKED_ROWS {
        return None;
    }
    let feature = DecisionFeature::CodeContext;
    let floor = feature.rerank_threshold(&decisions.config().thresholds)?;
    let candidates: Vec<Value> = rows[..head]
        .iter()
        .map(|row| candidate(&describe(row)))
        .collect();
    let reranked = decisions
        .rerank(
            Rerank {
                feature,
                relevance: &RELEVANCE,
                query: &json!({TASK_KEY: redacted_excerpt(task, MAX_TASK_BYTES)}),
                candidates: &candidates,
            },
            context,
            permissions,
            cancel,
        )
        .await?;
    let (mut order, below) = reranked.split_at_floor(floor);
    order.extend(below);
    if !reranked.record_rerouted(&order, head) {
        return None;
    }
    let mut original: Vec<Option<T>> = rows.drain(..head).map(Some).collect();
    rows.splice(
        0..0,
        order.iter().filter_map(|index| original[*index].take()),
    );
    Some(reordered_note(head))
}

pub fn reordered_note(rows: usize) -> String {
    format!(
        "[The first {rows} rows were reordered by decision-engine relevance to the task; the confidence above describes the code graph's own ranking.]"
    )
}

fn candidate(symbol: &CodeSymbol<'_>) -> Value {
    json!({
        SYMBOL_KEY: candidate_text(symbol.name, MAX_NAME_BYTES),
        KIND_KEY: candidate_text(symbol.kind, MAX_KIND_BYTES),
        PATH_KEY: candidate_text(symbol.path, MAX_PATH_BYTES),
        LINES_KEY: [symbol.line_start, symbol.line_end],
        CALLERS_KEY: symbol.callers,
        CALLEES_KEY: symbol.calls,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_storage::StateDir;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CALLEES_KEY, CALLERS_KEY, CodeSymbol, KIND_KEY, LINES_KEY, MAX_RERANK_BATCH, PATH_KEY,
        SYMBOL_KEY, TASK_KEY, reordered_note, rerank_code_context,
    };
    use crate::CancelToken;
    use crate::decisions::rerank::CANDIDATE_KEY;
    use crate::decisions::{DecisionContext, Decisions};

    const BASE_URL: &str = "http://127.0.0.1:1";
    const FLOOR: f64 = 0.1;
    const TASK: &str = "where do we invalidate the cache after a write";
    const SECRET: &str = "do-not-send-this-value";
    const KIND: &str = "function";
    const PATH: &str = "src/store/cache.rs";
    const LINE_START: usize = 12;
    const LINE_END: usize = 40;
    const CALLERS: usize = 3;
    const CALLS: usize = 5;
    const SHORT: usize = 5;
    const LONG: usize = MAX_RERANK_BATCH + 4;

    struct ScoringEngine {
        scores: BTreeMap<usize, f64>,
        fail: bool,
        requests: Arc<Mutex<Vec<DecisionRequest>>>,
    }

    #[async_trait]
    impl DecisionEngine for ScoringEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.requests.lock().unwrap().push(request.clone());
            if self.fail {
                return Err(DecisionError::Unreachable);
            }
            let answers = request
                .questions
                .keys()
                .enumerate()
                .map(|(index, id)| {
                    let noul = self.scores.get(&index).copied().unwrap_or_default();
                    (id.clone(), Answer::Noul(NoulAnswer { noul }))
                })
                .collect();
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                cache_hit: false,
            })
        }
    }

    struct Fixture {
        decisions: Decisions,
        requests: Arc<Mutex<Vec<DecisionRequest>>>,
        _root: TempDir,
    }

    impl Fixture {
        fn new(mode: FeatureMode, scores: &[(usize, f64)], fail: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut config = DecisionsConfig {
                base_url: Some(BASE_URL.parse().unwrap()),
                ..Default::default()
            };
            config.features.code_context = mode;
            config.thresholds.relevance_floor = FLOOR;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let decisions = Decisions::with_engine(
                config,
                &StateDir::from_path(root.path().into()),
                ScoringEngine {
                    scores: scores.iter().copied().collect(),
                    fail,
                    requests: requests.clone(),
                },
            )
            .unwrap();
            Self {
                decisions,
                requests,
                _root: root,
            }
        }

        fn rerank(
            &self,
            rows: &mut Vec<String>,
            task: &str,
            cancel: &CancelToken,
        ) -> Option<String> {
            smol::block_on(rerank_code_context(
                rows,
                |name| CodeSymbol {
                    name,
                    kind: KIND,
                    path: PATH,
                    line_start: LINE_START,
                    line_end: LINE_END,
                    callers: CALLERS,
                    calls: CALLS,
                },
                task,
                &self.decisions,
                &DecisionContext::default(),
                None,
                cancel,
            ))
        }

        fn requests(&self) -> Vec<DecisionRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn rows(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("symbol_{index:02}"))
            .collect()
    }

    #[test_case(&[(3, 0.9), (1, 0.3)], SHORT, &[3, 1]; "rows_above_the_floor_lead_and_the_rest_keep_workcell_order")]
    #[test_case(&[(4, FLOOR), (2, 0.05)], SHORT, &[4]; "the_floor_is_inclusive")]
    #[test_case(&[(1, 0.6), (12, 0.9)], LONG, &[12, 1]; "only_the_head_moves")]
    fn relevant_head_rows_move_first(scores: &[(usize, f64)], count: usize, lead: &[usize]) {
        let fixture = Fixture::new(FeatureMode::Enforce, scores, false);
        let mut reranked = rows(count);
        let note = fixture.rerank(&mut reranked, TASK, &CancelToken::none());
        let head = count.min(MAX_RERANK_BATCH);
        let original = rows(count);
        let expected: Vec<String> = lead
            .iter()
            .copied()
            .chain((0..count).filter(|index| !lead.contains(index)))
            .map(|index| original[index].clone())
            .collect();
        assert_eq!(reranked, expected);
        assert_eq!(note, Some(reordered_note(head)));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), head);
    }

    #[test_case(FeatureMode::Enforce, &[(1, 0.05)], SHORT, false, 1; "everything_below_the_floor")]
    #[test_case(FeatureMode::Enforce, &[(0, 0.9), (1, 0.5)], SHORT, false, 1; "already_in_workcell_order")]
    #[test_case(FeatureMode::Enforce, &[(0, 0.9)], 1, false, 0; "a_single_row")]
    #[test_case(FeatureMode::Enforce, &[(3, 0.9)], SHORT, true, 1; "engine_failure")]
    #[test_case(FeatureMode::Off, &[(3, 0.9)], SHORT, false, 0; "off")]
    #[test_case(FeatureMode::Shadow, &[(3, 0.9)], SHORT, false, 1; "shadow")]
    fn workcell_order_is_kept_without_a_change(
        mode: FeatureMode,
        scores: &[(usize, f64)],
        count: usize,
        fail: bool,
        requests: usize,
    ) {
        let fixture = Fixture::new(mode, scores, fail);
        let mut reranked = rows(count);
        assert_eq!(
            fixture.rerank(&mut reranked, TASK, &CancelToken::none()),
            None
        );
        assert_eq!(reranked, rows(count));
        assert_eq!(fixture.requests().len(), requests);
    }

    #[test]
    fn a_cancelled_call_keeps_workcell_order() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(3, 0.9)], false);
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let mut reranked = rows(SHORT);
        assert_eq!(fixture.rerank(&mut reranked, TASK, &cancel), None);
        assert_eq!(reranked, rows(SHORT));
        assert!(fixture.requests().is_empty());
    }

    #[test]
    fn candidates_carry_only_location_and_counts() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[], false);
        let mut reranked = rows(SHORT);
        fixture.rerank(
            &mut reranked,
            &format!("{TASK} TOKEN={SECRET}"),
            &CancelToken::none(),
        );
        let requests = fixture.requests();
        let request = &requests[0];
        assert!(!request.state.to_string().contains(SECRET));
        assert!(request.state[TASK_KEY].as_str().unwrap().starts_with(TASK));
        assert_eq!(request.state.as_object().unwrap().len(), 1);
        for (question, name) in request.questions.values().zip(rows(SHORT)) {
            assert_eq!(
                question.instructions[CANDIDATE_KEY],
                json!({
                    SYMBOL_KEY: name,
                    KIND_KEY: KIND,
                    PATH_KEY: PATH,
                    LINES_KEY: [LINE_START, LINE_END],
                    CALLERS_KEY: CALLERS,
                    CALLEES_KEY: CALLS,
                })
            );
        }
    }
}
