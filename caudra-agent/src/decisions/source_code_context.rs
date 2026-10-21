use caudra_config::decisions::FeatureMode;
use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, Decisions, NoteDraft, NoteOutcome, RERANK_POOL, Relevance,
    Rerank, candidate_text, placement, redacted_excerpt, score_line,
};
use crate::CancelToken;
use crate::permissions::PermissionManager;

const KEPT_WORKCELL_ORDER: &str = "kept Workcell's order";
const OUT_OF_DEADLINE: &str = "ran out of the call's deadline";
const LEFT_OUT: &str = "left out";
const MIN_RERANKED_ROWS: usize = 2;
/// Most rows judged unrelated that the model's note names; the rest are counted.
const MAX_NAMED_OMISSIONS: usize = 8;
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

/// Judges the first [`RERANK_POOL`] rows against `task` and, when enforcing, keeps
/// only those at or above the relevance floor, most relevant first. Rows judged
/// unrelated and rows past the pool are left out. When none clears the floor,
/// Workcell's order stands, since an empty answer helps no one. Returns the note
/// for the model when the rows changed, and the transcript row for a change, a
/// shadow change or a fresh failure.
pub async fn rerank_source_code_context<T>(
    rows: &mut Vec<T>,
    describe: impl Fn(&T) -> CodeSymbol<'_>,
    task: &str,
    decisions: &Decisions,
    context: &DecisionContext,
    permissions: Option<&PermissionManager>,
    cancel: &CancelToken,
) -> (Option<String>, Option<NoteDraft>) {
    let total = rows.len();
    let head = total.min(RERANK_POOL);
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
    let (kept, unrelated) = reranked.split_at_floor(floor);
    if kept.is_empty() || !reranked.record_rerouted(&kept, total) {
        return (None, None);
    }
    let label = |index: usize| {
        let symbol = describe(&rows[index]);
        format!("{}:{} {}", symbol.path, symbol.line_start, symbol.name)
    };
    let scores = reranked.scores();
    let moved_rows = kept
        .iter()
        .enumerate()
        .filter(|(to, from)| to < from)
        .map(|(to, from)| score_line(scores[*from], &label(*from), &placement(*from, to)));
    let left_out_rows = unrelated
        .iter()
        .map(|index| score_line(scores[*index], &label(*index), LEFT_OUT));
    let details: Vec<String> = moved_rows.chain(left_out_rows).collect();
    let moved = details.len() - unrelated.len();
    let pruning = Pruning {
        total,
        head,
        kept: kept.len(),
        unrelated: unrelated.len(),
    };
    let note = Some(NoteDraft::new(
        decisions,
        feature,
        outcome,
        &pruning.summary(moved),
        details,
    ));
    if outcome != NoteOutcome::Acted {
        return (None, note);
    }
    let named: Vec<String> = unrelated
        .iter()
        .take(MAX_NAMED_OMISSIONS)
        .map(|index| label(*index))
        .collect();
    let model_note = pruning.model_note(&named);
    let mut original: Vec<Option<T>> = rows.drain(..).map(Some).collect();
    rows.extend(kept.iter().filter_map(|index| original[*index].take()));
    (Some(model_note), note)
}

/// How a rerank changed `total` rows: it judged the first `head`, kept `kept`
/// and left out `unrelated` of them, and left out the rest unjudged.
struct Pruning {
    total: usize,
    head: usize,
    kept: usize,
    unrelated: usize,
}

impl Pruning {
    fn unjudged(&self) -> usize {
        self.total - self.head
    }

    /// `moved 3 of 12 rows up`, or `kept 12 of 200 rows; left out 52 judged
    /// unrelated; left out 136 past the first 64, not judged`.
    fn summary(&self, moved: usize) -> String {
        if self.kept == self.total {
            return format!("moved {moved} of {} rows up", self.head);
        }
        let mut summary = format!("kept {} of {} rows", self.kept, self.total);
        if self.unrelated > 0 {
            summary.push_str(&format!("; left out {} judged unrelated", self.unrelated));
        }
        if self.unjudged() > 0 {
            summary.push_str(&format!(
                "; left out {} past the first {}, not judged",
                self.unjudged(),
                self.head
            ));
        }
        summary
    }

    /// What the model reads after the rows: what was judged, kept and left
    /// out, naming the first rows judged unrelated.
    fn model_note(&self, named: &[String]) -> String {
        let (total, head) = (self.total, self.head);
        if self.kept == total {
            return reordered_note(head);
        }
        let judged = match head == total {
            true => format!("all {total} rows"),
            false => format!("the first {head} of {total} rows"),
        };
        let mut note = format!(
            "[Decision-engine relevance to the task judged {judged} and kept {}, best first.",
            self.kept
        );
        if self.unrelated > 0 {
            note.push_str(&format!(" Left out as unrelated: {}", named.join(", ")));
            if self.unrelated > named.len() {
                note.push_str(&format!(" and {} more", self.unrelated - named.len()));
            }
            note.push('.');
        }
        if self.unjudged() > 0 {
            note.push_str(&format!(
                " The {} rows after the first {head} were not judged and are left out.",
                self.unjudged()
            ));
        }
        note.push_str(" The confidence above describes the source-code graph's own ranking.]");
        note
    }
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
        MAX_NAMED_OMISSIONS, OUT_OF_DEADLINE, PATH_KEY, SYMBOL_KEY, TASK_KEY, reordered_note,
        rerank_source_code_context, source_code_context_out_of_deadline,
    };
    use crate::CancelToken;
    use crate::decisions::rerank::CANDIDATE_KEY;
    use crate::decisions::{
        DecisionContext, Decisions, ENGINE_UNREACHABLE, NoteDraft, NoteOutcome, RERANK_POOL,
    };

    const BASE_URL: &str = "http://127.0.0.1:1";
    const FLOOR: f64 = 0.1;
    const TASK: &str = "where do we invalidate the cache after a write";
    const SECRET: &str = "do-not-send-this-value";
    const MOVED_ONE: &str = "moved 1 of 5 rows up";
    const KEPT_ONE: &str = "kept 1 of 5 rows; left out 4 judged unrelated";
    const SHADOW_KEPT_ONE: &str = "would have kept 1 of 5 rows; left out 4 judged unrelated";
    const KEPT_TWO_OF_LONG: &str =
        "kept 2 of 68 rows; left out 62 judged unrelated; left out 4 past the first 64, not judged";
    const MOVED_ROW: &str = "0.90  src/store/cache.rs:12 symbol_03  moved 4th to 1st";
    const LEFT_OUT_ROW: &str = "0.00  src/store/cache.rs:12 symbol_00  left out";
    const IN_ORDER: &[(usize, f64)] = &[(0, 0.9), (1, 0.8), (2, 0.7), (3, 0.6), (4, 0.5)];
    const REORDERED: &[(usize, f64)] = &[(0, 0.5), (1, 0.5), (2, 0.5), (3, 0.9), (4, 0.5)];
    const KIND: &str = "function";
    const PATH: &str = "src/store/cache.rs";
    const LINE_START: usize = 12;
    const LINE_END: usize = 40;
    const CALLERS: usize = 3;
    const CALLS: usize = 5;
    const SHORT: usize = 5;
    const LONG: usize = RERANK_POOL + 4;

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

    fn label(index: usize) -> String {
        format!("{PATH}:{LINE_START} symbol_{index:02}")
    }

    #[test_case(&[(3, 0.9), (1, 0.3)], SHORT, &[3, 1]; "only_rows_at_or_above_the_floor_are_kept_best_first")]
    #[test_case(&[(4, FLOOR), (2, 0.05)], SHORT, &[4]; "the_floor_is_inclusive")]
    #[test_case(&[(1, 0.6), (12, 0.9)], LONG, &[12, 1]; "rows_past_the_pool_are_left_out_unjudged")]
    #[test_case(REORDERED, SHORT, &[3, 0, 1, 2, 4]; "every_row_relevant_is_a_reorder")]
    fn enforce_keeps_the_relevant_rows(scores: &[(usize, f64)], count: usize, kept: &[usize]) {
        let fixture = Fixture::new(FeatureMode::Enforce, scores, false);
        let mut reranked = rows(count);
        let (note, row) = fixture.rerank(&mut reranked, TASK, &CancelToken::none());
        let original = rows(count);
        let expected: Vec<String> = kept.iter().map(|index| original[*index].clone()).collect();
        assert_eq!(reranked, expected);
        assert!(note.is_some());
        assert_eq!(row.map(|row| row.outcome), Some(NoteOutcome::Acted));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), count.min(RERANK_POOL));
    }

    #[test_case(REORDERED, SHORT, MOVED_ONE, &[MOVED_ROW]; "a_reorder_names_each_row_that_moved_up")]
    #[test_case(&[(3, 0.9)], SHORT, KEPT_ONE, &[MOVED_ROW, LEFT_OUT_ROW]; "a_prune_names_the_rows_it_left_out")]
    fn a_row_says_what_changed(
        scores: &[(usize, f64)],
        count: usize,
        summary: &str,
        leading: &[&str],
    ) {
        let fixture = Fixture::new(FeatureMode::Enforce, scores, false);
        let (_, row) = fixture.rerank(&mut rows(count), TASK, &CancelToken::none());
        let row = row.unwrap();
        assert_eq!(row.summary, summary);
        assert_eq!(&row.details[..leading.len()], leading);
    }

    #[test]
    fn a_reorder_tells_the_model_only_that_rows_moved() {
        let fixture = Fixture::new(FeatureMode::Enforce, REORDERED, false);
        let (note, _) = fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
        assert_eq!(note, Some(reordered_note(SHORT)));
    }

    #[test]
    fn a_prune_tells_the_model_what_it_left_out() {
        let fixture = Fixture::new(FeatureMode::Enforce, &[(1, 0.6), (12, 0.9)], false);
        let (note, row) = fixture.rerank(&mut rows(LONG), TASK, &CancelToken::none());
        let unrelated = RERANK_POOL - 2;
        let named: Vec<String> = (0..RERANK_POOL)
            .filter(|index| ![1, 12].contains(index))
            .take(MAX_NAMED_OMISSIONS)
            .map(label)
            .collect();
        assert_eq!(
            note.unwrap(),
            format!(
                "[Decision-engine relevance to the task judged the first {RERANK_POOL} of {LONG} rows and kept 2, best first. Left out as unrelated: {} and {} more. The {} rows after the first {RERANK_POOL} were not judged and are left out. The confidence above describes the source-code graph's own ranking.]",
                named.join(", "),
                unrelated - MAX_NAMED_OMISSIONS,
                LONG - RERANK_POOL,
            )
        );
        assert_eq!(row.unwrap().summary, KEPT_TWO_OF_LONG);
    }

    #[test_case(FeatureMode::Enforce, &[(1, 0.05)], SHORT, false, 1, None; "everything_below_the_floor")]
    #[test_case(FeatureMode::Enforce, IN_ORDER, SHORT, false, 1, None; "already_in_workcell_order")]
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
    fn a_shadow_row_says_what_it_would_have_kept() {
        let fixture = Fixture::new(FeatureMode::Shadow, &[(3, 0.9)], false);
        let (_, row) = fixture.rerank(&mut rows(SHORT), TASK, &CancelToken::none());
        let row = row.unwrap();
        assert_eq!(row.summary, SHADOW_KEPT_ONE);
        assert_eq!(row.details[..2], [MOVED_ROW, LEFT_OUT_ROW]);
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
