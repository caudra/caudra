//! Searching stored session history, for the `code` tool's `history` module and any frontend.
//!
//! `caudra-storage` owns the keyword index. This adds what needs the agent: catching the index up
//! before a query, and the funnel that lets the decision engine rerank the keyword matches.

mod filters;
mod funnel;
mod project;

use std::time::{Duration, Instant};

use caudra_storage::StateDir;
use caudra_storage::history_search::{CatchUp, HistoryScope};
use caudra_storage::sessions::{SessionDatabase, SessionError};
use jiff::Timestamp;
use jiff::tz::TimeZone;

pub use filters::{QueryFilters, parse_filters, parse_iso_time};
pub use funnel::{
    HistoryResults, HistorySearch, LEXICAL_LIMIT, MAX_LIMIT, MAX_POOL_PER_SESSION, PoolJudgement,
    RERANK_POOL, RERANKED_LIMIT, Ranking, judge_pool, search,
};
pub use project::{HistoryProject, remote_identity};

/// How long a query waits for the index to catch up with its scope before it searches what is
/// indexed and counts the rest as unindexed.
pub const QUERY_CATCH_UP_BUDGET: Duration = Duration::from_millis(1_500);
const MILLIS_PER_SECOND: u64 = 1_000;

/// Where the history lives. Queries open the database read-only; only catching the index up
/// writes. Every call runs off the executor.
#[derive(Clone)]
pub struct HistoryStore {
    state: StateDir,
}

impl HistoryStore {
    pub fn new(state: StateDir) -> Self {
        Self { state }
    }

    /// Indexes what changed in `scope`, for at most [`QUERY_CATCH_UP_BUDGET`].
    pub async fn catch_up(&self, scope: HistoryScope) -> Result<CatchUp, SessionError> {
        let state = self.state.clone();
        smol::unblock(move || {
            SessionDatabase::open_state(&state)?
                .catch_up_history(&scope, Instant::now() + QUERY_CATCH_UP_BUDGET)
        })
        .await
    }

    pub async fn read<T: Send + 'static>(
        &self,
        query: impl FnOnce(&SessionDatabase) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let state = self.state.clone();
        smol::unblock(move || query(&SessionDatabase::open_read_only(&state)?)).await
    }
}

/// `2026-10-16T09:30:00Z`: Unix milliseconds as ISO-8601 UTC, to the second.
pub fn iso_time(at_ms: u64) -> String {
    timestamp(at_ms).to_string()
}

/// `2026-10-16`: the UTC day of Unix milliseconds.
pub fn iso_date(at_ms: u64) -> String {
    timestamp(at_ms).to_zoned(TimeZone::UTC).date().to_string()
}

fn timestamp(at_ms: u64) -> Timestamp {
    i64::try_from(at_ms / MILLIS_PER_SECOND)
        .ok()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
        .unwrap_or(Timestamp::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use caudra_providers::HistoryItem;
    use caudra_storage::id::CaudraId;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::stored_session::StoredSession;

    const DAY_MS: u64 = 1_767_225_600_000;
    const MODEL: &str = "test/model";
    const TITLE: &str = "Seeded";
    const MINUTE_MS: u64 = 60_000;
    const TIMESTAMP_BYTES: usize = 6;
    const MILLIS_BYTES: usize = 8;
    /// Seeding indexes everything up front, so no search in a test races the query's own budget.
    const SEED_INDEX_BUDGET: Duration = Duration::from_secs(60);

    /// The `index`th of a run of seeded items, each a minute after the last.
    pub(crate) fn stamped(base_ms: u64, index: usize) -> u64 {
        base_ms + MINUTE_MS * index as u64
    }

    /// A message matching `query` that names itself by `index`.
    pub(crate) fn seeded_text(query: &str, index: usize) -> String {
        format!("{query} {index}")
    }

    /// Saves one session in `cwd` under the state directory `root`, holding a user message per
    /// `(at_ms, text)`, and returns its id.
    pub(crate) fn seed(root: &Path, cwd: &str, items: &[(u64, String)]) -> CaudraId {
        let mut session = StoredSession::new(MODEL, cwd);
        session.set_title(TITLE.to_owned());
        session.replace_messages(
            items
                .iter()
                .map(|(at_ms, text)| {
                    let id = id_at(*at_ms).to_string();
                    serde_json::from_value::<HistoryItem>(json!({
                        "id": id,
                        "group_id": id,
                        "type": "user",
                        "text": text,
                        "origin": "turn",
                    }))
                    .unwrap()
                })
                .collect(),
        );
        let mut database = SessionDatabase::open_state(&StateDir::from_path(root.into())).unwrap();
        database.save(&session, None).unwrap();
        database
            .catch_up_history(&HistoryScope::All, Instant::now() + SEED_INDEX_BUDGET)
            .unwrap();
        session.id
    }

    /// A fresh UUIDv7 stamped `at_ms`.
    fn id_at(at_ms: u64) -> CaudraId {
        let mut bytes = *CaudraId::generate().as_bytes();
        bytes[..TIMESTAMP_BYTES]
            .copy_from_slice(&at_ms.to_be_bytes()[MILLIS_BYTES - TIMESTAMP_BYTES..]);
        CaudraId::from_bytes(bytes)
    }

    #[test_case(DAY_MS, "2026-01-01T00:00:00Z"; "midnight")]
    #[test_case(DAY_MS + 1_999, "2026-01-01T00:00:01Z"; "drops_the_milliseconds")]
    fn times_read_as_utc_seconds(at_ms: u64, expected: &str) {
        assert_eq!(iso_time(at_ms), expected);
    }

    #[test]
    fn dates_read_as_the_utc_day() {
        assert_eq!(iso_date(DAY_MS - 1), "2025-12-31");
    }
}
