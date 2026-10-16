use caudra_config::decisions::FeatureMode;
use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, Decisions, MAX_RERANK_BATCH, NoteDraft, NoteOutcome,
    Relevance, Rerank, candidate_text, placement, redacted_excerpt, score_line,
};
use crate::CancelToken;
use crate::permissions::PermissionManager;

const KEPT_WORKCELL_ORDER: &str = "kept Workcell's order";
const OUT_OF_DEADLINE: &str = "ran out of the call's deadline";
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

/// One `source_code_context` row as the decision engine sees it: where the symbol
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
/// never move. Returns the note for the model when the order changed, and the
/// transcript row for a move, a shadow move or a fresh failure.
pub async fn rerank_source_code_context<T>(
    rows: &mut Vec<T>,
    describe: impl Fn(&T) -> CodeSymbol<'_>,
    task: &str,
    decisions: &Decisions,
    context: &DecisionContext,
    permissions: Option<&PermissionManager>,
    cancel: &CancelToken,
) -> (Option<String>, Option<NoteDraft>) {
    let head = rows.len().min(MAX_RERANK_BATCH);
    let feature = DecisionFeature::SourceCodeContext;
    let Some(floor) = feature
        .rerank_threshold(&decisions.config().thresholds)
        .filter(|_| head >= MIN_RERANKED_ROWS)
    else {
        return (None, None);
    };
    let candidates: Vec<Value> = rows[..head]
        .iter()
        .map(|row| candidate(&describe(row)))
        .collect();
    let reranking = decisions
        .rerank(
            Rerank {
                feature: feature.clone(),
                relevance: &RELEVANCE,
                query: &json!({TASK_KEY: redacted_excerpt(task, MAX_TASK_BYTES)}),
                candidates: &candidates,
            },
            context,
            permissions,
            cancel,
        )
        .await;
    let (reranked, outcome) = match reranking.scored() {
        Ok(scored) => scored,
        Err(failure) => {
            let note = failure.map(|failure| {
                NoteDraft::fell_back(decisions, feature, &failure, KEPT_WORKCELL_ORDER)
            });
            return (None, note);
        }
    };
    let (mut order, below) = reranked.split_at_floor(floor);
    order.extend(below);
    if !reranked.record_rerouted(&order, head) {
        return (None, None);
    }
    let moved: Vec<String> = order
        .iter()
        .enumerate()
        .filter(|(to, from)| to < from)
        .map(|(to, from)| {
            let symbol = describe(&rows[*from]);
            score_line(
                reranked.scores()[*from],
                &format!("{}:{} {}", symbol.path, symbol.line_start, symbol.name),
                &placement(*from, to),
            )
        })
        .collect();
    let summary = format!("moved {} of {head} rows up", moved.len());
    let note = Some(NoteDraft::new(decisions, feature, outcome, &summary, moved));
    if outcome != NoteOutcome::Acted {
        return (None, note);
    }
    let mut original: Vec<Option<T>> = rows.drain(..head).map(Some).collect();
    rows.splice(
        0..0,
        order.iter().filter_map(|index| original[*index].take()),
    );
    (Some(reordered_note(head)), note)
}

/// The row for a rerank the call's own deadline cut short, reported like an
/// engine failure: once, until the feature answers again.
pub fn source_code_context_out_of_deadline(decisions: &Decisions) -> Option<NoteDraft> {
    let feature = DecisionFeature::SourceCodeContext;
    let applied = matches!(
        decisions.mode(&feature),
        FeatureMode::Advise | FeatureMode::Enforce
    );
    (applied && decisions.mark_failing(&feature))
        .then(|| NoteDraft::fell_back(decisions, feature, OUT_OF_DEADLINE, KEPT_WORKCELL_ORDER))
}

pub fn reordered_note(rows: usize) -> String {
    format!(
        "[The first {rows} rows were reordered by decision-engine relevance to the task; the confidence above describes the source-code graph's own ranking.]"
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
    use caudra_config::decisions::{DecisionEndpoint, DecisionProtocol};
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
        CALLEES_KEY, CALLERS_KEY, CodeSymbol, KEPT_WORKCELL_ORDER, KIND_KEY, LINES_KEY,
        MAX_RERANK_BATCH, OUT_OF_DEADLINE, PATH_KEY, SYMBOL_KEY, TASK_KEY, reordered_note,
        rerank_source_code_context, source_code_context_out_of_deadline,
    };
    use crate::CancelToken;
    use crate::decisions::rerank::CANDIDATE_KEY;
    use crate::decisions::{
        DecisionContext, Decisions, ENGINE_UNREACHABLE, NoteDraft, NoteOutcome,
    };

    const BASE_URL: &str = "http://127.0.0.1:1";
    const FLOOR: f64 = 0.1;
    const TASK: &str = "where do we invalidate the cache after a write";
    const SECRET: &str = "do-not-send-this-value";
    const MOVED_ONE: &str = "moved 1 of 5 rows up";
    const SHADOW_MOVED_ONE: &str = "would have moved 1 of 5 rows up";
    const MOVED_ROW: &str = "0.90  src/store/cache.rs:12 symbol_03  moved 4th to 1st";
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
                endpoint: Some(DecisionEndpoint::new(
                    DecisionProtocol::TypeSafe.as_str(),
                    DecisionProtocol::TypeSafe,
                    BASE_URL.parse().unwrap(),
                )),
                ..Default::default()
            };
            config.features.source_code_context = mode;
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
        ) -> (Option<String>, Option<NoteDraft>) {
            smol::block_on(rerank_source_code_context(
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
        let (note, row) = fixture.rerank(&mut reranked, TASK, &CancelToken::none());
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
        assert_eq!(row.map(|row| row.outcome), Some(NoteOutcome::Acted));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), head);
    }

    #[test]
    fn a_move_names_each_row_that_moved_up() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(3, 0.9)], false);
        let (_, row) = fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
        let row = row.unwrap();
        assert_eq!(row.summary, MOVED_ONE);
        assert_eq!(row.details, [MOVED_ROW]);
    }

    #[test_case(FeatureMode::Enforce, &[(1, 0.05)], SHORT, false, 1, None; "everything_below_the_floor")]
    #[test_case(FeatureMode::Enforce, &[(0, 0.9), (1, 0.5)], SHORT, false, 1, None; "already_in_workcell_order")]
    #[test_case(FeatureMode::Enforce, &[(0, 0.9)], 1, false, 0, None; "a_single_row")]
    #[test_case(FeatureMode::Enforce, &[(3, 0.9)], SHORT, true, 1, Some(NoteOutcome::FellBack); "engine_failure")]
    #[test_case(FeatureMode::Off, &[(3, 0.9)], SHORT, false, 0, None; "off")]
    #[test_case(FeatureMode::Shadow, &[(3, 0.9)], SHORT, false, 1, Some(NoteOutcome::Shadow); "shadow")]
    fn workcell_order_is_kept_without_a_change(
        mode: FeatureMode,
        scores: &[(usize, f64)],
        count: usize,
        fail: bool,
        requests: usize,
        outcome: Option<NoteOutcome>,
    ) {
        let fixture = Fixture::new(mode, scores, fail);
        let mut reranked = rows(count);
        let (note, row) = fixture.rerank(&mut reranked, TASK, &CancelToken::none());
        assert_eq!(note, None);
        assert_eq!(row.map(|row| row.outcome), outcome);
        assert_eq!(reranked, rows(count));
        assert_eq!(fixture.requests().len(), requests);
    }

    #[test]
    fn a_shadow_row_says_what_would_have_moved() {
        let fixture = Fixture::new(FeatureMode::Shadow, &[(3, 0.9)], false);
        let (_, row) = fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
        let row = row.unwrap();
        assert_eq!(row.summary, SHADOW_MOVED_ONE);
        assert_eq!(row.details, [MOVED_ROW]);
    }

    #[test]
    fn a_failing_engine_falls_back_once_until_it_answers() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(3, 0.9)], true);
        let summaries: Vec<Option<String>> = (0..2)
            .map(|_| {
                let (_, row) = fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
                row.map(|row| row.summary)
            })
            .collect();
        assert_eq!(
            summaries,
            [
                Some(format!("{ENGINE_UNREACHABLE}; {KEPT_WORKCELL_ORDER}")),
                None
            ]
        );
    }

    #[test]
    fn a_cancelled_call_keeps_workcell_order() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(3, 0.9)], false);
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let mut reranked = rows(SHORT);
        assert_eq!(fixture.rerank(&mut reranked, TASK, &cancel), (None, None));
        assert_eq!(reranked, rows(SHORT));
        assert!(fixture.requests().is_empty());
    }

    #[test_case(FeatureMode::Enforce, &[Some(NoteOutcome::FellBack), None, Some(NoteOutcome::FellBack)]; "enforce_reports_once_per_outage")]
    #[test_case(FeatureMode::Shadow, &[None, None, None]; "shadow_never_falls_back")]
    fn a_deadline_cut_rerank_falls_back_once_until_it_answers(
        mode: FeatureMode,
        expected: &[Option<NoteOutcome>],
    ) {
        let fixture = Fixture::new(mode, &[(3, 0.9)], false);
        let first = source_code_context_out_of_deadline(&fixture.decisions);
        let second = source_code_context_out_of_deadline(&fixture.decisions);
        fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
        let after_an_answer = source_code_context_out_of_deadline(&fixture.decisions);
        let outcomes: Vec<Option<NoteOutcome>> = [&first, &second, &after_an_answer]
            .map(|row| row.as_ref().map(|row| row.outcome))
            .into();
        assert_eq!(outcomes, expected);
        if let Some(row) = first {
            assert_eq!(
                row.summary,
                format!("{OUT_OF_DEADLINE}; {KEPT_WORKCELL_ORDER}")
            );
        }
    }

    #[test]
    fn a_row_never_carries_the_task_or_a_secret_in_a_label() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(3, 0.9)], false);
        let mut reranked = rows(SHORT);
        reranked[3] = format!("TOKEN={SECRET}");
        let (_, row) = fixture.rerank(
            &mut reranked,
            &format!("{TASK} TOKEN={SECRET}"),
            &CancelToken::none(),
        );
        let row = row.unwrap();
        let text = format!("{} {}", row.summary, row.details.join("\n"));
        assert!(!text.contains(SECRET));
        assert!(!text.contains(TASK));
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
