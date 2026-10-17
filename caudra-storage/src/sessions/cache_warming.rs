//! What prompt-cache warming cost and what it saved, one row per episode: from
//! a request that armed warming to the next request on the same conversation,
//! or to the moment warming stopped. A `shadow` episode is what warming would
//! have done while it was off, so the two can be compared before turning it on.
//!
//! Like `usage_ledger`, rows name a session but never cascade with it: spend
//! must outlive the transcript that caused it.
//!
//! A shadow episode cannot observe whether its warms would have kept the cache,
//! because the real request after the gap missed either way. Its savings are
//! stored as the full rewrite it would have avoided and weighted at read time
//! by `cache_hit_baseline`: how often a request that arrived inside the cache
//! lifetime actually hit, on the same model and at the same kind of boundary.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::sessions::{SessionDatabase, SessionError, cache_hit_rate, from_i64, to_i64};
use crate::{StateClass, StateDir};

/// Within-lifetime pairs a model needs before its hit rate weights a shadow
/// estimate. Fewer leave the estimate uncalibrated rather than guessed.
pub const MIN_BASELINE_PAIRS: u64 = 20;
const STARTED_AT_FIELD: &str = "cache_warm_episodes.started_at";
const ENDED_AT_FIELD: &str = "cache_warm_episodes.ended_at";
const GAP_FIELD: &str = "cache_warm_episodes.gap_ms";
const COUNT_FIELD: &str = "cache_warm_episodes.count";
const WARMS_FIELD: &str = "cache_warm_episodes.warms";
const PAIRS_FIELD: &str = "cache_hit_baseline.pairs";
const HITS_FIELD: &str = "cache_hit_baseline.hits";

pub(crate) const TABLES: &str = r#"
CREATE TABLE cache_warm_episodes (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL,
    agent           TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK(kind IN ('real', 'shadow')),
    mode            TEXT NOT NULL,
    provider        TEXT NOT NULL,
    account         TEXT NOT NULL,
    model           TEXT NOT NULL,
    cwd             TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    ended_at        INTEGER NOT NULL,
    gap_ms          INTEGER,
    boundary        TEXT CHECK(boundary IN ('within_run', 'new_run')),
    prompt_tokens   INTEGER NOT NULL,
    warms           INTEGER NOT NULL,
    input_tokens    INTEGER NOT NULL,
    cache_read      INTEGER NOT NULL,
    cache_creation  INTEGER NOT NULL,
    warm_cost       REAL NOT NULL,
    subscription    INTEGER NOT NULL CHECK(subscription IN (0, 1)),
    outcome         TEXT NOT NULL,
    next_cache_read INTEGER,
    saved_tokens    INTEGER NOT NULL,
    saved_cost      REAL NOT NULL,
    stop_reason     TEXT NOT NULL
) STRICT;
CREATE INDEX cache_warm_episodes_session ON cache_warm_episodes(session_id);
CREATE TABLE cache_hit_baseline (
    provider TEXT NOT NULL,
    model    TEXT NOT NULL,
    boundary TEXT NOT NULL CHECK(boundary IN ('within_run', 'new_run')),
    pairs    INTEGER NOT NULL,
    hits     INTEGER NOT NULL,
    PRIMARY KEY(provider, model, boundary)
) STRICT, WITHOUT ROWID;
"#;

macro_rules! storage_names {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn from_storage_name(value: &str) -> Option<Self> {
                match value {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

/// Whether the warms were sent or only simulated while warming was off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    Real,
    Shadow,
}

storage_names!(EpisodeKind { Real => "real", Shadow => "shadow" });

/// Where the request that ended an episode sat: answering a tool result inside
/// the run, or opening the next run. The second misses more often for reasons
/// no warm can fix, which is why the baseline keeps them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    WithinRun,
    NewRun,
}

storage_names!(Boundary { WithinRun => "within_run", NewRun => "new_run" });

/// How an episode with at least one warm ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// The cache would have expired, and the next request read it.
    Saved,
    /// The next request read the cache within the lifetime's fuzzy edge, where
    /// it might have hit without any warm. Never credited.
    Marginal,
    /// The next request came inside the lifetime, so no warm was needed.
    Unneeded,
    /// The next request missed although the warms kept the entry.
    Missed,
    /// Warming stopped, and the next request came after the entry expired.
    Lapsed,
    /// No next request read the prefix: the run ended, the conversation changed,
    /// or the session closed.
    Unused,
    /// A warm wrote a new entry instead of refreshing the existing one.
    Wrote,
}

storage_names!(EpisodeOutcome {
    Saved => "saved",
    Marginal => "marginal",
    Unneeded => "unneeded",
    Missed => "missed",
    Lapsed => "lapsed",
    Unused => "unused",
    Wrote => "wrote",
});

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheWarmEpisode {
    pub session_id: String,
    /// `main`, or the tool use id of the subagent the conversation belongs to.
    pub agent: String,
    pub kind: EpisodeKind,
    /// The mode in force, or the one a shadow episode simulated.
    pub mode: String,
    pub provider: String,
    pub account: String,
    pub model: String,
    pub cwd: String,
    /// Unix milliseconds of the request that armed the episode.
    pub started_at: u64,
    pub ended_at: u64,
    /// From the arming request's start to the next request's start.
    pub gap_ms: Option<u64>,
    pub boundary: Option<Boundary>,
    pub prompt_tokens: u32,
    pub warms: u32,
    pub input_tokens: u32,
    pub cache_read: u32,
    pub cache_creation: u32,
    pub warm_cost: f64,
    pub subscription: bool,
    pub outcome: EpisodeOutcome,
    pub next_cache_read: Option<u32>,
    pub saved_tokens: u32,
    /// The rewrite avoided. For a shadow episode, the rewrite it would have
    /// avoided had the cache hit, before the baseline weights it.
    pub saved_cost: f64,
    pub stop_reason: String,
}

/// Within-lifetime request pairs a session observed, folded into the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HitSamples {
    pub provider: String,
    pub model: String,
    pub boundary: Boundary,
    pub pairs: u64,
    pub hits: u64,
}

/// One kind and mode's totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WarmingTotals {
    pub episodes: u64,
    pub warms: u64,
    pub warm_cost: f64,
    /// For a shadow report, weighted by the baseline hit rate.
    pub saved_cost: f64,
    /// Shadow savings left out because their model has too few baseline pairs.
    pub uncalibrated_cost: f64,
    pub outcomes: BTreeMap<EpisodeOutcome, u64>,
}

impl WarmingTotals {
    pub fn net(&self) -> f64 {
        self.saved_cost - self.warm_cost
    }

    /// Of the next requests that came after the entry would have expired, the
    /// share that read it, which only the warms can explain.
    pub fn post_warm_hit_rate(&self) -> Option<f64> {
        let count = |outcome| self.outcomes.get(&outcome).copied().unwrap_or(0);
        let hits = count(EpisodeOutcome::Saved) + count(EpisodeOutcome::Marginal);
        cache_hit_rate(hits, hits + count(EpisodeOutcome::Missed))
    }
}

/// Totals keyed by kind and mode name.
pub type WarmingSummary = BTreeMap<(EpisodeKind, String), WarmingTotals>;

/// A handle for the warming tables, opened on the persistent root like the
/// usage ledger so an ephemeral session's spend is not lost with it.
pub struct CacheWarmLedger {
    database: SessionDatabase,
}

impl CacheWarmLedger {
    pub fn open(dir: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?,
        })
    }

    pub fn record_episode(&self, episode: &CacheWarmEpisode) -> Result<(), SessionError> {
        self.database.connection().execute(
            "INSERT INTO cache_warm_episodes (session_id, agent, kind, mode, provider, account, \
             model, cwd, started_at, ended_at, gap_ms, boundary, prompt_tokens, warms, \
             input_tokens, cache_read, cache_creation, warm_cost, subscription, outcome, \
             next_cache_read, saved_tokens, saved_cost, stop_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
             ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
            params![
                episode.session_id,
                episode.agent,
                episode.kind.as_str(),
                episode.mode,
                episode.provider,
                episode.account,
                episode.model,
                episode.cwd,
                to_i64(episode.started_at, STARTED_AT_FIELD)?,
                to_i64(episode.ended_at, ENDED_AT_FIELD)?,
                episode
                    .gap_ms
                    .map(|gap| to_i64(gap, GAP_FIELD))
                    .transpose()?,
                episode.boundary.map(Boundary::as_str),
                episode.prompt_tokens,
                episode.warms,
                episode.input_tokens,
                episode.cache_read,
                episode.cache_creation,
                episode.warm_cost,
                episode.subscription,
                episode.outcome.as_str(),
                episode.next_cache_read,
                episode.saved_tokens,
                episode.saved_cost,
                episode.stop_reason,
            ],
        )?;
        Ok(())
    }

    pub fn record_samples(&self, samples: &[HitSamples]) -> Result<(), SessionError> {
        let mut upsert = self.database.connection().prepare_cached(
            "INSERT INTO cache_hit_baseline (provider, model, boundary, pairs, hits) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(provider, model, boundary) DO UPDATE SET \
             pairs = pairs + excluded.pairs, hits = hits + excluded.hits",
        )?;
        for sample in samples {
            upsert.execute(params![
                sample.provider,
                sample.model,
                sample.boundary.as_str(),
                to_i64(sample.pairs, PAIRS_FIELD)?,
                to_i64(sample.hits, HITS_FIELD)?,
            ])?;
        }
        Ok(())
    }

    /// The share of within-lifetime requests that hit, once a model has
    /// [`MIN_BASELINE_PAIRS`] of them at `boundary`.
    pub fn hit_rate(
        &self,
        provider: &str,
        model: &str,
        boundary: Boundary,
    ) -> Result<Option<f64>, SessionError> {
        let row: Option<(i64, i64)> = self
            .database
            .connection()
            .query_row(
                "SELECT pairs, hits FROM cache_hit_baseline \
                 WHERE provider = ?1 AND model = ?2 AND boundary = ?3",
                params![provider, model, boundary.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((pairs, hits)) = row else {
            return Ok(None);
        };
        let pairs = from_i64(pairs, PAIRS_FIELD)?;
        let hits = from_i64(hits, HITS_FIELD)?;
        Ok((pairs >= MIN_BASELINE_PAIRS).then(|| hits as f64 / pairs as f64))
    }

    /// Totals per kind and mode, for one session or for every episode.
    pub fn summary(&self, session_id: Option<&str>) -> Result<WarmingSummary, SessionError> {
        let mut statement = self.database.connection().prepare(
            "SELECT kind, mode, outcome, provider, model, boundary, count(*), sum(warms), \
             sum(warm_cost), sum(saved_cost) FROM cache_warm_episodes \
             WHERE ?1 IS NULL OR session_id = ?1 \
             GROUP BY kind, mode, outcome, provider, model, boundary",
        )?;
        let mut rows = statement.query(params![session_id])?;
        let mut summary = WarmingSummary::new();
        while let Some(row) = rows.next()? {
            let (Some(kind), Some(outcome)) = (
                EpisodeKind::from_storage_name(&row.get::<_, String>(0)?),
                EpisodeOutcome::from_storage_name(&row.get::<_, String>(2)?),
            ) else {
                continue;
            };
            let mode: String = row.get(1)?;
            let provider: String = row.get(3)?;
            let model: String = row.get(4)?;
            let boundary = row
                .get::<_, Option<String>>(5)?
                .as_deref()
                .and_then(Boundary::from_storage_name);
            let episodes = from_i64(row.get(6)?, COUNT_FIELD)?;
            let warms = from_i64(row.get(7)?, WARMS_FIELD)?;
            let saved: f64 = row.get(9)?;
            let totals = summary.entry((kind, mode)).or_default();
            totals.episodes += episodes;
            totals.warms += warms;
            totals.warm_cost += row.get::<_, f64>(8)?;
            *totals.outcomes.entry(outcome).or_default() += episodes;
            match (kind, boundary) {
                (EpisodeKind::Real, _) => totals.saved_cost += saved,
                (EpisodeKind::Shadow, Some(boundary)) if outcome == EpisodeOutcome::Saved => {
                    match self.hit_rate(&provider, &model, boundary)? {
                        Some(rate) => totals.saved_cost += saved * rate,
                        None => totals.uncalibrated_cost += saved,
                    }
                }
                (EpisodeKind::Shadow, _) => {}
            }
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const SESSION: &str = "session-a";
    const OTHER_SESSION: &str = "session-b";
    const PROVIDER: &str = "anthropic";
    const MODEL: &str = "claude-opus-5-5";
    const RUNNING: &str = "running";
    const AGENT: &str = "main";
    const CWD: &str = "/project";
    const WARM_COST: f64 = 0.03;
    const SAVED_COST: f64 = 0.72;
    const WITHIN_RUN_PAIRS: u64 = 40;
    const WITHIN_RUN_HITS: u64 = 30;
    const FLOAT_TOLERANCE: f64 = 1e-9;
    const REAL_SAVINGS_COUNT: &str = "a real episode's savings count in full";
    const SHADOW_WEIGHTED: &str = "a shadow saving is weighted by the baseline hit rate";
    const SHADOW_UNCALIBRATED: &str =
        "a shadow saving without enough baseline pairs stays out of the estimate";
    const SESSION_FILTER: &str = "a session summary counts only that session's episodes";
    const BASELINE_ACCUMULATES: &str = "samples add to the baseline instead of replacing it";

    fn ledger() -> (TempDir, CacheWarmLedger) {
        let temp = TempDir::new().unwrap();
        let ledger =
            CacheWarmLedger::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, ledger)
    }

    fn episode(session_id: &str, kind: EpisodeKind, outcome: EpisodeOutcome) -> CacheWarmEpisode {
        CacheWarmEpisode {
            session_id: session_id.into(),
            agent: AGENT.into(),
            kind,
            mode: RUNNING.into(),
            provider: PROVIDER.into(),
            account: String::new(),
            model: MODEL.into(),
            cwd: CWD.into(),
            started_at: 1_000,
            ended_at: 400_000,
            gap_ms: Some(399_000),
            boundary: Some(Boundary::WithinRun),
            prompt_tokens: 150_000,
            warms: 1,
            input_tokens: 0,
            cache_read: 150_000,
            cache_creation: 0,
            warm_cost: WARM_COST,
            subscription: true,
            outcome,
            next_cache_read: Some(150_000),
            saved_tokens: 150_000,
            saved_cost: SAVED_COST,
            stop_reason: String::new(),
        }
    }

    fn samples(pairs: u64, hits: u64) -> HitSamples {
        HitSamples {
            provider: PROVIDER.into(),
            model: MODEL.into(),
            boundary: Boundary::WithinRun,
            pairs,
            hits,
        }
    }

    fn totals(summary: &WarmingSummary, kind: EpisodeKind) -> &WarmingTotals {
        &summary[&(kind, RUNNING.to_owned())]
    }

    #[test]
    fn real_episodes_count_their_savings_in_full() {
        let (_temp, ledger) = ledger();
        ledger
            .record_episode(&episode(SESSION, EpisodeKind::Real, EpisodeOutcome::Saved))
            .unwrap();

        let summary = ledger.summary(None).unwrap();
        let real = totals(&summary, EpisodeKind::Real);
        assert!(
            (real.net() - (SAVED_COST - WARM_COST)).abs() < FLOAT_TOLERANCE,
            "{REAL_SAVINGS_COUNT}"
        );
        assert_eq!(
            real.outcomes[&EpisodeOutcome::Saved],
            1,
            "{REAL_SAVINGS_COUNT}"
        );
    }

    #[test_case(WITHIN_RUN_PAIRS, WITHIN_RUN_HITS, SAVED_COST * 0.75, 0.0; "calibrated")]
    #[test_case(MIN_BASELINE_PAIRS - 1, 1, 0.0, SAVED_COST; "too_few_pairs")]
    fn shadow_savings_are_weighted_by_the_baseline(
        pairs: u64,
        hits: u64,
        saved: f64,
        uncalibrated: f64,
    ) {
        let (_temp, ledger) = ledger();
        ledger.record_samples(&[samples(pairs, hits)]).unwrap();
        ledger
            .record_episode(&episode(
                SESSION,
                EpisodeKind::Shadow,
                EpisodeOutcome::Saved,
            ))
            .unwrap();

        let summary = ledger.summary(None).unwrap();
        let shadow = totals(&summary, EpisodeKind::Shadow);
        assert!(
            (shadow.saved_cost - saved).abs() < FLOAT_TOLERANCE,
            "{SHADOW_WEIGHTED}"
        );
        assert!(
            (shadow.uncalibrated_cost - uncalibrated).abs() < FLOAT_TOLERANCE,
            "{SHADOW_UNCALIBRATED}"
        );
    }

    #[test]
    fn a_session_summary_counts_only_its_own_episodes() {
        let (_temp, ledger) = ledger();
        for session in [SESSION, OTHER_SESSION] {
            ledger
                .record_episode(&episode(session, EpisodeKind::Real, EpisodeOutcome::Unused))
                .unwrap();
        }

        let summary = ledger.summary(Some(SESSION)).unwrap();
        assert_eq!(
            totals(&summary, EpisodeKind::Real).episodes,
            1,
            "{SESSION_FILTER}"
        );
        let lifetime = ledger.summary(None).unwrap();
        assert_eq!(
            totals(&lifetime, EpisodeKind::Real).episodes,
            2,
            "{SESSION_FILTER}"
        );
    }

    #[test]
    fn samples_accumulate_in_the_baseline() {
        let (_temp, ledger) = ledger();
        ledger
            .record_samples(&[samples(WITHIN_RUN_PAIRS / 2, WITHIN_RUN_HITS / 2)])
            .unwrap();
        ledger
            .record_samples(&[samples(WITHIN_RUN_PAIRS / 2, WITHIN_RUN_HITS / 2)])
            .unwrap();

        let rate = ledger
            .hit_rate(PROVIDER, MODEL, Boundary::WithinRun)
            .unwrap();
        assert_eq!(
            rate,
            Some(WITHIN_RUN_HITS as f64 / WITHIN_RUN_PAIRS as f64),
            "{BASELINE_ACCUMULATES}"
        );
    }

    #[test_case(&[], None; "nothing_came_after_expiry")]
    #[test_case(&[EpisodeOutcome::Unneeded, EpisodeOutcome::Unused], None; "only_unneeded_and_unused")]
    #[test_case(&[EpisodeOutcome::Saved, EpisodeOutcome::Marginal, EpisodeOutcome::Missed, EpisodeOutcome::Missed], Some(0.5); "hits_over_late_requests")]
    fn post_warm_hit_rate_counts_only_requests_after_expiry(
        outcomes: &[EpisodeOutcome],
        expected: Option<f64>,
    ) {
        let mut totals = WarmingTotals::default();
        for outcome in outcomes {
            *totals.outcomes.entry(*outcome).or_default() += 1;
        }
        assert_eq!(totals.post_warm_hit_rate(), expected);
    }

    #[test_case(EpisodeOutcome::Saved)]
    #[test_case(EpisodeOutcome::Marginal)]
    #[test_case(EpisodeOutcome::Unneeded)]
    #[test_case(EpisodeOutcome::Missed)]
    #[test_case(EpisodeOutcome::Lapsed)]
    #[test_case(EpisodeOutcome::Unused)]
    #[test_case(EpisodeOutcome::Wrote)]
    fn outcomes_round_trip_their_storage_names(outcome: EpisodeOutcome) {
        assert_eq!(
            EpisodeOutcome::from_storage_name(outcome.as_str()),
            Some(outcome)
        );
    }
}
