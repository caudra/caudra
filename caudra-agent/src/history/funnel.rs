//! The rerank funnel. The keyword ranker supplies a pool, the decision engine drops the matches it
//! judges unrelated, and the rest come back best first under a small top-k. Without an answer to
//! apply (no engine, the feature off or in shadow, a failure, or `rerank` turned off) the search
//! is the keyword ranker's own, under a larger top-k.

use caudra_storage::history_search::{HistoryHit, HistoryQuery, HistorySort};
use caudra_storage::sessions::SessionError;
use serde_json::{Value, json};
use tracing::warn;

use super::{HistoryStore, iso_date};
use crate::decisions::{
    DecisionFeature, NoteDraft, NoteOutcome, Relevance, Rerank, Reranked, candidate_text, counted,
    placement, redacted_excerpt, score_line,
};
use crate::memory::relevance::Judge;

/// One request's worth of keyword matches; BM25 matches past about 50 are mostly single-term
/// noise in conversational text anyway.
pub use crate::decisions::RERANK_POOL;

/// At most this many pool slots for one session, so one long session never fills the pool.
/// Applies only to scopes that span sessions.
pub const MAX_POOL_PER_SESSION: usize = 8;
/// The page size after reranking.
pub const RERANKED_LIMIT: usize = 10;
/// The page size in keyword order.
pub const LEXICAL_LIMIT: usize = 20;
/// The largest page, which stays within [`RERANK_POOL`].
pub const MAX_LIMIT: usize = 50;
const CANDIDATE_SNIPPET_BYTES: usize = 600;
const TITLE_BYTES: usize = 160;
const TOOL_BYTES: usize = 80;
const QUERY_BYTES: usize = 400;
/// Raw text is cut to this many times its limit before redaction, which then sees whole secrets
/// and never scans more than a window.
const WINDOW_FACTOR: usize = 2;
const QUERY_KEY: &str = "query";
const KIND_KEY: &str = "kind";
const TOOL_KEY: &str = "tool";
const DATE_KEY: &str = "date";
const TITLE_KEY: &str = "session_title";
const EXCERPT_KEY: &str = "excerpt";
const KEPT_KEYWORD_ORDER: &str = "kept the keyword order";
const KEYWORD_MATCH_ONE: &str = "keyword match";
const KEYWORD_MATCH_MANY: &str = "keyword matches";
const NOT_SHOWN: &str = "not shown";
const LEFT_OUT: &str = "left out";
const FELL_BACK_NOTE: &str = "The reranker did not answer, so these hits are in keyword order.";
const RELEVANCE: Relevance = Relevance {
    statement: "The past conversation excerpt helps answer the history search `query`.",
    relevant: "It discusses, decides or shows what the query looks for.",
    unrelated: "It only shares words with the query, or is about something else.",
};

pub struct HistorySearch {
    /// What to match. Its `limit` and `per_session_cap` are the funnel's to set.
    pub query: HistoryQuery,
    /// `false` keeps the keyword order even while the feature enforces.
    pub rerank: bool,
    /// The page size; by default [`RERANKED_LIMIT`] after reranking and [`LEXICAL_LIMIT`]
    /// without, and never above [`MAX_LIMIT`].
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ranking {
    /// The engine judged a `pool` of keyword matches and `kept` those that cleared the floor.
    Reranked { pool: usize, kept: usize },
    /// The keyword order, out of `matched` distinct matches.
    Keyword { matched: usize },
}

#[derive(Debug)]
pub struct HistoryResults {
    /// After reranking, each hit's score is the engine's relevance; in keyword order, BM25's.
    pub hits: Vec<HistoryHit>,
    pub ranking: Ranking,
    /// More hits follow this page: kept ones within the pool after reranking.
    pub more: bool,
    /// Sessions in scope the index had not caught up with when the search ran.
    pub unindexed_sessions: usize,
    /// What the caller should know about the ranking, such as every match judged unrelated.
    pub note: Option<String>,
    /// The row for what the engine changed, would have changed in shadow, or newly failed at.
    pub decision: Option<NoteDraft>,
}

impl HistoryResults {
    pub fn judged_unrelated(&self) -> usize {
        match self.ranking {
            Ranking::Reranked { pool, kept } => pool - kept,
            Ranking::Keyword { .. } => 0,
        }
    }

    /// `reranked 64 keyword matches, kept 12, showing 10` or `keyword order: 20 of 143`.
    pub fn summary(&self) -> String {
        match self.ranking {
            Ranking::Reranked { pool, kept } => format!(
                "reranked {}, kept {kept}, showing {}",
                counted(pool, KEYWORD_MATCH_ONE, KEYWORD_MATCH_MANY),
                self.hits.len()
            ),
            Ranking::Keyword { matched } => {
                format!("keyword order: {} of {matched}", self.hits.len())
            }
        }
    }
}

/// How the engine judged a pool, whether or not its feature applies the judgement.
pub struct PoolJudgement {
    reranked: Reranked,
    outcome: NoteOutcome,
    /// At or above the relevance floor: most relevant first for a relevance search, ties in
    /// keyword order, and in time order for a newest-first one.
    pub kept: Vec<usize>,
    /// Below the floor, in pool order.
    pub omitted: Vec<usize>,
}

impl PoolJudgement {
    pub fn applied(&self) -> bool {
        self.outcome == NoteOutcome::Acted
    }

    /// Records `Rerouted` when the first `end` kept matches differ from the keyword order's, and
    /// then the row saying so.
    fn row(&self, pool: &[HistoryHit], end: usize) -> Option<NoteDraft> {
        let shown: Vec<usize> = self.kept.iter().copied().take(end).collect();
        self.reranked
            .record_rerouted(&shown, pool.len().min(end))
            .then(|| {
                let mut acted = format!(
                    "ranked {}",
                    counted(pool.len(), KEYWORD_MATCH_ONE, KEYWORD_MATCH_MANY)
                );
                if !self.omitted.is_empty() {
                    acted.push_str(&format!(
                        "; left out {} judged unrelated",
                        self.omitted.len()
                    ));
                }
                NoteDraft::new(
                    self.reranked.decisions(),
                    DecisionFeature::HistorySearch,
                    self.outcome,
                    &acted,
                    self.lines(pool, end),
                )
            })
    }

    /// Every match, kept ones first in their new order: its score, label and where it went.
    fn lines(&self, pool: &[HistoryHit], end: usize) -> Vec<String> {
        let scores = self.reranked.scores();
        let kept = self.kept.iter().enumerate().map(|(position, index)| {
            let action = match position < end {
                true => placement(*index, position),
                false => NOT_SHOWN.to_owned(),
            };
            (*index, action)
        });
        let omitted = self
            .omitted
            .iter()
            .map(|index| (*index, LEFT_OUT.to_owned()));
        kept.chain(omitted)
            .map(|(index, action)| score_line(scores[index], &label(&pool[index]), &action))
            .collect()
    }
}

/// Searches stored history, catching the index up with the scope first. `judge` reranks when
/// `search.rerank` asks for it and the `history_search` feature is on.
pub async fn search(
    store: &HistoryStore,
    search: HistorySearch,
    judge: Option<&Judge>,
) -> Result<HistoryResults, SessionError> {
    let HistorySearch {
        mut query,
        rerank,
        limit,
    } = search;
    let unindexed_sessions = match store.catch_up(query.scope.clone()).await {
        Ok(catch_up) => catch_up.pending_sessions,
        Err(error) => {
            warn!(%error, "history search could not catch the index up");
            0
        }
    };
    let judge = judge.filter(|judge| {
        rerank && !query.text.trim().is_empty() && judge.enabled(&DecisionFeature::HistorySearch)
    });
    let mut decision = None;
    if let Some(judge) = judge {
        let pool_query = HistoryQuery {
            limit: RERANK_POOL,
            offset: 0,
            per_session_cap: Some(MAX_POOL_PER_SESSION),
            ..query.clone()
        };
        let pool = store
            .read(move |database| database.search_history(&pool_query))
            .await?
            .hits;
        let limit = limit.unwrap_or(RERANKED_LIMIT).min(MAX_LIMIT);
        let end = query.offset.saturating_add(limit);
        match judge_pool(judge, &query.text, &pool, query.sort).await {
            Ok(judgement) if judgement.applied() => {
                return Ok(reranked(
                    judgement,
                    pool,
                    query.offset,
                    limit,
                    unindexed_sessions,
                ));
            }
            Ok(judgement) => decision = judgement.row(&pool, end),
            Err(row) => decision = row,
        }
    }
    let fell_back = decision
        .as_ref()
        .is_some_and(|row| row.outcome == NoteOutcome::FellBack);
    query.limit = limit.unwrap_or(LEXICAL_LIMIT).min(MAX_LIMIT);
    query.per_session_cap = None;
    let page = store
        .read(move |database| database.search_history(&query))
        .await?;
    Ok(HistoryResults {
        hits: page.hits,
        ranking: Ranking::Keyword {
            matched: page.matched,
        },
        more: page.more,
        unindexed_sessions,
        note: fell_back.then(|| FELL_BACK_NOTE.to_owned()),
        decision,
    })
}

/// Asks the engine about each match in `pool` against `query`. `Err` carries the row for a fresh
/// failure, when there is one.
pub async fn judge_pool(
    judge: &Judge,
    query: &str,
    pool: &[HistoryHit],
    sort: HistorySort,
) -> Result<PoolJudgement, Option<NoteDraft>> {
    let feature = DecisionFeature::HistorySearch;
    let floor = feature
        .rerank_threshold(&judge.decisions.config().thresholds)
        .ok_or(None)?;
    let state = json!({QUERY_KEY: redacted_excerpt(query, QUERY_BYTES)});
    let candidates: Vec<Value> = pool.iter().map(candidate).collect();
    let (reranked, outcome) = judge
        .decisions
        .rerank(
            Rerank {
                feature: feature.clone(),
                relevance: &RELEVANCE,
                query: &state,
                candidates: &candidates,
            },
            &judge.context,
            judge.permissions.as_deref(),
            &judge.cancel,
        )
        .await
        .scored()
        .map_err(|failure| {
            failure.map(|failure| {
                NoteDraft::fell_back(&judge.decisions, feature, &failure, KEPT_KEYWORD_ORDER)
            })
        })?;
    let (mut kept, omitted) = reranked.split_at_floor(floor);
    if sort == HistorySort::Newest {
        kept.sort_unstable();
    }
    Ok(PoolJudgement {
        reranked,
        outcome,
        kept,
        omitted,
    })
}

fn reranked(
    judgement: PoolJudgement,
    pool: Vec<HistoryHit>,
    offset: usize,
    limit: usize,
    unindexed_sessions: usize,
) -> HistoryResults {
    let end = offset.saturating_add(limit);
    let decision = judgement.row(&pool, end);
    let ranking = Ranking::Reranked {
        pool: pool.len(),
        kept: judgement.kept.len(),
    };
    let note = (judgement.kept.is_empty() && !judgement.omitted.is_empty()).then(|| {
        format!(
            "All {} were judged unrelated to the query. Rephrase it, or pass rerank=False for keyword order.",
            counted(pool.len(), KEYWORD_MATCH_ONE, KEYWORD_MATCH_MANY)
        )
    });
    let scores = judgement.reranked.scores();
    let mut slots: Vec<Option<HistoryHit>> = pool.into_iter().map(Some).collect();
    let hits = judgement
        .kept
        .iter()
        .skip(offset)
        .take(limit)
        .filter_map(|index| {
            slots[*index].take().map(|mut hit| {
                hit.score = scores[*index];
                hit
            })
        })
        .collect();
    HistoryResults {
        hits,
        ranking,
        more: judgement.kept.len() > end,
        unindexed_sessions,
        note,
        decision,
    }
}

fn candidate(hit: &HistoryHit) -> Value {
    json!({
        KIND_KEY: hit.kind.as_str(),
        TOOL_KEY: hit.tool.as_deref().map(|tool| bounded(tool, TOOL_BYTES)),
        DATE_KEY: iso_date(hit.at_ms),
        TITLE_KEY: bounded(&hit.session_title, TITLE_BYTES),
        EXCERPT_KEY: bounded(&hit.snippet.text, CANDIDATE_SNIPPET_BYTES),
    })
}

fn bounded(text: &str, limit: usize) -> String {
    candidate_text(
        &text[..text.floor_char_boundary(limit * WINDOW_FACTOR)],
        limit,
    )
}

/// `2026-10-16 user Release prep`: a match as a row names it.
fn label(hit: &HistoryHit) -> String {
    format!("{} {} {}", iso_date(hit.at_ms), hit.kind, hit.session_title)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use caudra_config::decisions::FeatureMode;
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::DecisionEffect;
    use caudra_storage::history_search::{
        HistoryKind, HistoryRef, HistoryScope, HistoryStream, Snippet,
    };
    use caudra_storage::id::CaudraId;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::history::tests::{seed, seeded_text, stamped};
    use crate::memory::relevance::tests::{
        NoteEngine, RELATED, RELEVANT, assert_effects, note_judge,
    };

    const PROJECT: &str = "/work/project";
    const QUERY: &str = "retry";
    const BASE_MS: u64 = 1_767_225_600_000;
    const SECRET: &str = "do-not-send-this-value";
    const TOKEN_PREFIX: &str = "TOKEN=";
    const REQUESTS_FOR_A_FULL_POOL: usize = 1;

    struct Fixture {
        state: TempDir,
        store: HistoryStore,
        session: CaudraId,
    }

    /// One session holding `count` matching user messages, a minute apart, oldest first.
    fn fixture(count: usize) -> Fixture {
        let state = TempDir::new().unwrap();
        let items: Vec<(u64, String)> = (0..count)
            .map(|index| (stamped(BASE_MS, index), seeded_text(QUERY, index)))
            .collect();
        let session = seed(state.path(), PROJECT, &items);
        let store = HistoryStore::new(StateDir::from_path(state.path().into()));
        Fixture {
            state,
            store,
            session,
        }
    }

    fn judge(root: &Path, mode: FeatureMode, engine: NoteEngine) -> Judge {
        note_judge(root, |features| features.history_search = mode, engine)
    }

    fn request(fixture: &Fixture, sort: HistorySort, rerank: bool) -> HistorySearch {
        let mut query = HistoryQuery::new(QUERY, HistoryScope::Session(fixture.session));
        query.sort = sort;
        HistorySearch {
            query,
            rerank,
            limit: None,
        }
    }

    fn texts(results: &HistoryResults) -> Vec<String> {
        results
            .hits
            .iter()
            .map(|hit| hit.snippet.text.clone())
            .collect()
    }

    fn run(
        fixture: &Fixture,
        sort: HistorySort,
        rerank: bool,
        judge: Option<&Judge>,
    ) -> HistoryResults {
        smol::block_on(search(
            &fixture.store,
            request(fixture, sort, rerank),
            judge,
        ))
        .unwrap()
    }

    #[test]
    fn enforcing_keeps_what_clears_the_floor_best_first_and_records_the_change() {
        let fixture = fixture(RERANKED_LIMIT + 2);
        let scored: Vec<String> = (0..RERANKED_LIMIT + 1)
            .map(|index| seeded_text(QUERY, index))
            .collect();
        let mut scores: Vec<(&str, f64)> =
            scored.iter().map(|text| (text.as_str(), RELATED)).collect();
        scores[1].1 = RELEVANT;
        let judge = judge(
            fixture.state.path(),
            FeatureMode::Enforce,
            NoteEngine::scoring(&scores),
        );

        let results = run(&fixture, HistorySort::Relevance, true, Some(&judge));

        let shown = texts(&results);
        assert_eq!(shown.len(), RERANKED_LIMIT);
        assert_eq!(shown[0], scored[1]);
        assert!(!shown.contains(&seeded_text(QUERY, RERANKED_LIMIT + 1)));
        assert_eq!(
            results.ranking,
            Ranking::Reranked {
                pool: RERANKED_LIMIT + 2,
                kept: RERANKED_LIMIT + 1
            }
        );
        assert_eq!(results.judged_unrelated(), 1);
        assert!(results.more);
        assert_eq!(results.hits[0].score, RELEVANT);
        smol::block_on(assert_effects(
            fixture.state.path(),
            &[DecisionEffect::Rerouted],
        ));
    }

    #[test]
    fn a_newest_search_keeps_time_order_after_filtering() {
        let fixture = fixture(3);
        let (oldest, middle, newest) = (
            seeded_text(QUERY, 0),
            seeded_text(QUERY, 1),
            seeded_text(QUERY, 2),
        );
        let judge = judge(
            fixture.state.path(),
            FeatureMode::Enforce,
            NoteEngine::scoring(&[(&oldest, RELEVANT), (&newest, RELATED)]),
        );

        let results = run(&fixture, HistorySort::Newest, true, Some(&judge));

        assert_eq!(texts(&results), [newest, oldest]);
        assert!(!texts(&results).contains(&middle));
    }

    #[test_case(None, true; "no_engine")]
    #[test_case(Some(FeatureMode::Off), true; "feature_off")]
    #[test_case(Some(FeatureMode::Enforce), false; "rerank_turned_off")]
    fn without_reranking_the_keyword_order_shows_more(mode: Option<FeatureMode>, rerank: bool) {
        let fixture = fixture(LEXICAL_LIMIT + 2);
        let engine = NoteEngine::scoring(&[]);
        let calls = Arc::clone(&engine.calls);
        let judge = mode.map(|mode| judge(fixture.state.path(), mode, engine));

        let results = run(&fixture, HistorySort::Newest, rerank, judge.as_ref());

        assert_eq!(results.hits.len(), LEXICAL_LIMIT);
        assert_eq!(texts(&results)[0], seeded_text(QUERY, LEXICAL_LIMIT + 1));
        assert!(matches!(results.ranking, Ranking::Keyword { .. }));
        assert!(results.more);
        assert_eq!(results.note, None);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shadow_asks_but_keeps_the_keyword_order() {
        let fixture = fixture(3);
        let engine = NoteEngine::scoring(&[(&seeded_text(QUERY, 0), RELEVANT)]);
        let calls = Arc::clone(&engine.calls);
        let judge = judge(fixture.state.path(), FeatureMode::Shadow, engine);

        let results = run(&fixture, HistorySort::Newest, true, Some(&judge));

        assert_eq!(texts(&results)[0], seeded_text(QUERY, 2));
        assert!(matches!(results.ranking, Ranking::Keyword { .. }));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(results.decision.unwrap().outcome, NoteOutcome::Shadow);
    }

    #[test]
    fn a_failed_rerank_falls_back_to_keyword_order_with_a_note() {
        let fixture = fixture(3);
        let judge = judge(
            fixture.state.path(),
            FeatureMode::Enforce,
            NoteEngine {
                fail: true,
                ..NoteEngine::scoring(&[])
            },
        );

        let results = run(&fixture, HistorySort::Newest, true, Some(&judge));

        assert_eq!(results.hits.len(), 3);
        assert!(matches!(results.ranking, Ranking::Keyword { .. }));
        assert_eq!(results.note.as_deref(), Some(FELL_BACK_NOTE));
        assert_eq!(results.decision.unwrap().outcome, NoteOutcome::FellBack);
    }

    #[test]
    fn everything_judged_unrelated_leaves_no_hits_and_says_why() {
        let fixture = fixture(3);
        let judge = judge(
            fixture.state.path(),
            FeatureMode::Enforce,
            NoteEngine::scoring(&[]),
        );

        let results = run(&fixture, HistorySort::Relevance, true, Some(&judge));

        assert!(results.hits.is_empty());
        assert_eq!(results.judged_unrelated(), 3);
        assert!(results.note.unwrap().contains("rerank=False"));
    }

    #[test]
    fn a_full_pool_is_one_request() {
        let fixture = fixture(RERANK_POOL + MAX_POOL_PER_SESSION);
        let engine = NoteEngine::scoring(&[]);
        let calls = Arc::clone(&engine.calls);
        let judge = judge(fixture.state.path(), FeatureMode::Enforce, engine);

        let results = run(&fixture, HistorySort::Relevance, true, Some(&judge));

        assert_eq!(
            results.ranking,
            Ranking::Reranked {
                pool: RERANK_POOL,
                kept: 0
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), REQUESTS_FOR_A_FULL_POOL);
    }

    #[test]
    fn a_candidate_is_redacted_and_bounded() {
        let hit = HistoryHit {
            reference: HistoryRef {
                session: CaudraId::generate(),
                stream: HistoryStream::Main,
                item: CaudraId::generate(),
            },
            session_title: format!("{TOKEN_PREFIX}{SECRET}"),
            project: PROJECT.to_owned(),
            kind: HistoryKind::User,
            tool: None,
            at_ms: BASE_MS,
            score: 0.0,
            snippet: Snippet {
                text: format!(
                    "{TOKEN_PREFIX}{SECRET} {}",
                    "x".repeat(CANDIDATE_SNIPPET_BYTES * 2)
                ),
                highlights: Vec::new(),
            },
        };

        let candidate = candidate(&hit);

        assert!(!candidate.to_string().contains(SECRET));
        let excerpt = candidate[EXCERPT_KEY].as_str().unwrap();
        assert!(excerpt.len() <= CANDIDATE_SNIPPET_BYTES);
    }

    #[test_case(Ranking::Reranked { pool: RERANK_POOL, kept: 12 }, RERANKED_LIMIT, "reranked 64 keyword matches, kept 12, showing 10"; "reranked")]
    #[test_case(Ranking::Keyword { matched: 143 }, LEXICAL_LIMIT, "keyword order: 20 of 143"; "keyword")]
    fn a_summary_says_how_the_hits_were_ranked(ranking: Ranking, shown: usize, expected: &str) {
        let fixture = fixture(shown);
        let mut results = run(&fixture, HistorySort::Newest, false, None);
        results.ranking = ranking;

        assert_eq!(results.summary(), expected);
    }
}
