//! Lifetime spend, kept apart from the sessions that produced it.
//!
//! Forgetting a session erases its `model_usage` rows through a foreign key.
//! That is the right behaviour for a transcript and the wrong one for money, so
//! every turn also lands here, in an hourly bucket that names no session. The
//! ledger always writes to the persistent root: an ephemeral run keeps no
//! transcript but still spent real money, so its rows are recorded with
//! `ephemeral` set rather than discarded with the volatile directory.
//!
//! The write cannot share a transaction with the session save, because in an
//! ephemeral run the two target different files. A crash between them loses at
//! most the last turn's accounting.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::sessions::{LedgerEntry, SessionDatabase, SessionError, StoredTokenUsage, UsageBucket};
use crate::{StateClass, StateDir, now_epoch};

pub const BUCKET_SECONDS: u64 = 60 * 60;
const MONTH_FORMAT: &str = "%Y-%m";

/// Why a model was called. Chat is the conversation itself; the rest is what
/// Caudra spends on the user's behalf without being asked, which is exactly the
/// spend a bill is queried about. The model alone cannot answer that: a goal
/// evaluated by the chat model is indistinguishable from the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerPurpose {
    Chat,
    Goal,
    Compaction,
    Title,
    Btw,
}

impl LedgerPurpose {
    pub const fn storage_name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Goal => "goal",
            Self::Compaction => "compaction",
            Self::Title => "title",
            Self::Btw => "btw",
        }
    }

    pub fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "goal" => Some(Self::Goal),
            "compaction" => Some(Self::Compaction),
            "title" => Some(Self::Title),
            "btw" => Some(Self::Btw),
            _ => None,
        }
    }
}

/// One turn's contribution, before it is folded into its bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsage {
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub purpose: LedgerPurpose,
    pub input: u32,
    pub output: u32,
    pub cache_creation: u32,
    pub cache_read: u32,
    /// `None` when the model has no price. Counted as an unpriced turn so a
    /// lifetime total can say how much of itself is missing.
    pub cost: Option<f64>,
}

pub fn bucket_for(epoch_seconds: u64) -> i64 {
    (epoch_seconds - epoch_seconds % BUCKET_SECONDS) as i64
}

/// A handle for several ledger operations in a row. Recording holds one open
/// across a run rather than paying for a connection per turn.
pub struct UsageLedger {
    database: SessionDatabase,
    ephemeral: bool,
}

impl UsageLedger {
    /// The ledger lives with credentials and preferences, not with sessions, so
    /// an ephemeral run reaches the real root here rather than its volatile
    /// copy, and marks what it writes.
    pub fn open(dir: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?,
            ephemeral: dir.is_ephemeral(),
        })
    }

    pub fn record(&self, turn: &TurnUsage) -> Result<(), SessionError> {
        self.record_at(turn, now_epoch())
    }

    fn record_at(&self, turn: &TurnUsage, now: u64) -> Result<(), SessionError> {
        self.database.record_usage(&LedgerEntry {
            bucket_start: bucket_for(now),
            provider: &turn.provider,
            model: &turn.model,
            cwd: &turn.cwd,
            purpose: turn.purpose,
            ephemeral: self.ephemeral,
            usage: StoredTokenUsage {
                input: turn.input,
                output: turn.output,
                cache_creation: turn.cache_creation,
                cache_read: turn.cache_read,
                cost: turn.cost,
            },
            cost: turn.cost,
        })
    }

    pub fn buckets(&self, since: Option<i64>) -> Result<Vec<UsageBucket>, SessionError> {
        self.database.usage_buckets(since)
    }

    pub fn prune_before(&self, bucket_start: i64) -> Result<usize, SessionError> {
        self.database.prune_usage_before(bucket_start)
    }

    pub fn lifetime(&self) -> Result<LifetimeUsage, SessionError> {
        Ok(summarize(&self.buckets(None)?))
    }
}

/// One line of a lifetime breakdown: a model, a project, a purpose, or a month.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSlice {
    pub label: String,
    pub cost: f64,
    pub tokens: u64,
    pub turns: u64,
}

/// Everything recorded, folded four ways. Built from the whole table, so it
/// answers "what has this cost me" rather than "what has this session cost me".
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LifetimeUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost: f64,
    /// Spend from `--ephemeral` runs, already counted in `cost`.
    pub ephemeral_cost: f64,
    pub priced_turns: u64,
    /// Turns whose model had no price. `cost` understates by an unknown
    /// amount rather than by zero, and this is how much of it is unknown.
    pub unpriced_turns: u64,
    /// Start of the oldest bucket still recorded, so a total can say how far
    /// back it reaches.
    pub since: Option<i64>,
    pub by_model: Vec<UsageSlice>,
    pub by_project: Vec<UsageSlice>,
    pub by_purpose: Vec<UsageSlice>,
    pub by_month: Vec<UsageSlice>,
}

impl LifetimeUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
    }

    pub fn turns(&self) -> u64 {
        self.priced_turns.saturating_add(self.unpriced_turns)
    }

    pub fn is_empty(&self) -> bool {
        self.turns() == 0 && self.total_tokens() == 0
    }
}

fn summarize(buckets: &[UsageBucket]) -> LifetimeUsage {
    let mut summary = LifetimeUsage::default();
    let mut by_model: BTreeMap<String, UsageSlice> = BTreeMap::new();
    let mut by_project: BTreeMap<String, UsageSlice> = BTreeMap::new();
    let mut by_purpose: BTreeMap<String, UsageSlice> = BTreeMap::new();
    let mut by_month: BTreeMap<String, UsageSlice> = BTreeMap::new();
    for bucket in buckets {
        summary.input += bucket.input;
        summary.output += bucket.output;
        summary.cache_creation += bucket.cache_creation;
        summary.cache_read += bucket.cache_read;
        summary.cost += bucket.cost;
        summary.priced_turns += bucket.priced_turns;
        summary.unpriced_turns += bucket.unpriced_turns;
        if bucket.ephemeral {
            summary.ephemeral_cost += bucket.cost;
        }
        summary.since = Some(
            summary
                .since
                .map_or(bucket.bucket_start, |since| since.min(bucket.bucket_start)),
        );
        add_slice(
            &mut by_model,
            format!("{}/{}", bucket.provider, bucket.model),
            bucket,
        );
        add_slice(&mut by_project, bucket.cwd.clone(), bucket);
        add_slice(&mut by_purpose, bucket.purpose.clone(), bucket);
        add_slice(&mut by_month, month_label(bucket.bucket_start), bucket);
    }
    summary.by_model = ranked(by_model);
    summary.by_project = ranked(by_project);
    summary.by_purpose = ranked(by_purpose);
    summary.by_month = by_month.into_values().rev().collect();
    summary
}

fn add_slice(slices: &mut BTreeMap<String, UsageSlice>, label: String, bucket: &UsageBucket) {
    let slice = slices.entry(label.clone()).or_insert(UsageSlice {
        label,
        cost: 0.0,
        tokens: 0,
        turns: 0,
    });
    slice.cost += bucket.cost;
    slice.tokens += bucket.input + bucket.output + bucket.cache_creation + bucket.cache_read;
    slice.turns += bucket.priced_turns + bucket.unpriced_turns;
}

/// Dearest first: a spend breakdown is read to find where the money went.
fn ranked(slices: BTreeMap<String, UsageSlice>) -> Vec<UsageSlice> {
    let mut slices: Vec<UsageSlice> = slices.into_values().collect();
    slices.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.tokens.cmp(&a.tokens))
            .then_with(|| a.label.cmp(&b.label))
    });
    slices
}

fn month_label(bucket_start: i64) -> String {
    Timestamp::from_second(bucket_start).map_or_else(
        |_| bucket_start.to_string(),
        |timestamp| {
            timestamp
                .to_zoned(TimeZone::system())
                .strftime(MONTH_FORMAT)
                .to_string()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    const PROVIDER: &str = "anthropic";
    const MODEL: &str = "claude-opus-4";
    const OTHER_MODEL: &str = "claude-haiku-4";
    const CWD: &str = "/repo";
    const HOUR: u64 = BUCKET_SECONDS;
    const SURVIVES_DELETE: &str = "spend must outlive the session that produced it";
    const EPHEMERAL_PERSISTS: &str = "an ephemeral run spends real money and must record it";
    const NO_VOLATILE_LEDGER: &str = "the ledger belongs to the persistent root";
    const UNPRICED_COUNTED: &str = "an unpriced turn must be counted, not dropped";
    const PURPOSE_SEPARATES: &str = "spend Caudra makes on its own must stay tellable from the chat";

    fn turn(model: &str, cost: Option<f64>) -> TurnUsage {
        TurnUsage {
            provider: PROVIDER.into(),
            model: model.into(),
            cwd: CWD.into(),
            purpose: LedgerPurpose::Chat,
            input: 10,
            output: 20,
            cache_creation: 30,
            cache_read: 40,
            cost,
        }
    }

    fn ledger(dir: &StateDir) -> UsageLedger {
        UsageLedger::open(dir).unwrap()
    }

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    #[test]
    fn turns_in_one_hour_accumulate_into_a_single_row() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.5)), 0).unwrap();
        ledger(&dir)
            .record_at(&turn(MODEL, Some(2.5)), HOUR - 1)
            .unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input, 20);
        assert_eq!(rows[0].output, 40);
        assert_eq!(rows[0].cost, 4.0);
        assert_eq!(rows[0].priced_turns, 2);
    }

    #[test]
    fn a_later_hour_starts_its_own_row() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger(&dir)
            .record_at(&turn(MODEL, Some(1.0)), HOUR)
            .unwrap();

        assert_eq!(ledger(&dir).buckets(None).unwrap().len(), 2);
    }

    #[test]
    fn each_model_keeps_its_own_row() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger(&dir)
            .record_at(&turn(OTHER_MODEL, Some(1.0)), 0)
            .unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|row| row.cost).sum::<f64>(), 2.0);
    }

    #[test]
    fn one_model_in_one_hour_still_splits_by_purpose() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger(&dir)
            .record_at(
                &TurnUsage {
                    purpose: LedgerPurpose::Goal,
                    ..turn(MODEL, Some(4.0))
                },
                0,
            )
            .unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();

        assert_eq!(rows.len(), 2, "{PURPOSE_SEPARATES}");
        let goal = rows
            .iter()
            .find(|row| row.purpose == LedgerPurpose::Goal.storage_name())
            .expect(PURPOSE_SEPARATES);
        assert_eq!(goal.cost, 4.0, "{PURPOSE_SEPARATES}");
    }

    #[test_case(LedgerPurpose::Chat ; "chat")]
    #[test_case(LedgerPurpose::Goal ; "goal")]
    #[test_case(LedgerPurpose::Compaction ; "compaction")]
    #[test_case(LedgerPurpose::Title ; "title")]
    #[test_case(LedgerPurpose::Btw ; "btw")]
    fn a_purpose_round_trips_through_the_ledger(purpose: LedgerPurpose) {
        let (_temp, dir) = state_dir();
        ledger(&dir)
            .record_at(
                &TurnUsage {
                    purpose,
                    ..turn(MODEL, Some(1.0))
                },
                0,
            )
            .unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();

        assert_eq!(rows[0].purpose, purpose.storage_name());
        assert_eq!(
            LedgerPurpose::from_storage_name(&rows[0].purpose),
            Some(purpose)
        );
    }

    #[test]
    fn a_lifetime_summary_ranks_purposes_by_cost() {
        let (_temp, dir) = state_dir();
        let ledger = ledger(&dir);
        ledger.record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger
            .record_at(
                &TurnUsage {
                    purpose: LedgerPurpose::Goal,
                    ..turn(MODEL, Some(6.0))
                },
                0,
            )
            .unwrap();

        let lifetime = ledger.lifetime().unwrap();

        assert_eq!(
            lifetime
                .by_purpose
                .iter()
                .map(|slice| slice.label.as_str())
                .collect::<Vec<_>>(),
            [
                LedgerPurpose::Goal.storage_name(),
                LedgerPurpose::Chat.storage_name()
            ],
            "{PURPOSE_SEPARATES}"
        );
        assert_eq!(lifetime.by_purpose[0].cost, 6.0);
    }

    #[test]
    fn an_unpriced_turn_is_counted_rather_than_priced_at_zero() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(3.0)), 0).unwrap();
        ledger(&dir).record_at(&turn(MODEL, None), 0).unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();

        assert_eq!(rows[0].cost, 3.0, "{UNPRICED_COUNTED}");
        assert_eq!(rows[0].priced_turns, 1, "{UNPRICED_COUNTED}");
        assert_eq!(rows[0].unpriced_turns, 1, "{UNPRICED_COUNTED}");
        assert_eq!(rows[0].input, 20, "{UNPRICED_COUNTED}");
    }

    #[test]
    fn since_filters_older_buckets_out() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger(&dir)
            .record_at(&turn(MODEL, Some(1.0)), HOUR * 2)
            .unwrap();

        let rows = ledger(&dir).buckets(Some(bucket_for(HOUR * 2))).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_start, bucket_for(HOUR * 2));
    }

    #[test]
    fn pruning_drops_only_buckets_before_the_cutoff() {
        let (_temp, dir) = state_dir();
        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();
        ledger(&dir)
            .record_at(&turn(MODEL, Some(1.0)), HOUR * 2)
            .unwrap();

        assert_eq!(ledger(&dir).prune_before(bucket_for(HOUR * 2)).unwrap(), 1);
        assert_eq!(ledger(&dir).buckets(None).unwrap().len(), 1);
    }

    #[test]
    fn an_ephemeral_run_records_spend_in_the_persistent_root() {
        let temp = TempDir::new().unwrap();
        let persistent = temp.path().join("persistent");
        let volatile = temp.path().join("volatile");
        std::fs::create_dir_all(&persistent).unwrap();
        std::fs::create_dir_all(&volatile).unwrap();
        let dir = StateDir::split(volatile.clone(), persistent.clone());

        ledger(&dir).record_at(&turn(MODEL, Some(1.0)), 0).unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();
        assert_eq!(rows.len(), 1, "{EPHEMERAL_PERSISTS}");
        assert!(rows[0].ephemeral, "{EPHEMERAL_PERSISTS}");
        assert!(
            !volatile.join(crate::sessions::SESSIONS_DB_FILE).exists(),
            "{NO_VOLATILE_LEDGER}"
        );
        assert!(
            ledger(&StateDir::from_path(persistent))
                .buckets(None)
                .unwrap()
                .len()
                == 1,
            "{EPHEMERAL_PERSISTS}"
        );
    }

    #[test]
    fn a_lifetime_summary_ranks_models_and_projects_by_cost() {
        let (_temp, dir) = state_dir();
        let ledger = ledger(&dir);
        ledger.record_at(&turn(MODEL, Some(9.0)), 0).unwrap();
        ledger
            .record_at(
                &TurnUsage {
                    cwd: "/other".into(),
                    ..turn(OTHER_MODEL, Some(1.0))
                },
                0,
            )
            .unwrap();

        let lifetime = ledger.lifetime().unwrap();

        assert_eq!(lifetime.cost, 10.0);
        assert_eq!(lifetime.turns(), 2);
        assert_eq!(lifetime.by_model[0].label, format!("{PROVIDER}/{MODEL}"));
        assert_eq!(lifetime.by_project[0].label, CWD);
        assert_eq!(lifetime.since, Some(0));
    }

    #[test]
    fn a_lifetime_summary_keeps_unpriced_and_ephemeral_spend_visible() {
        let temp = TempDir::new().unwrap();
        let persistent = temp.path().join("persistent");
        let volatile = temp.path().join("volatile");
        std::fs::create_dir_all(&persistent).unwrap();
        std::fs::create_dir_all(&volatile).unwrap();
        let ephemeral = ledger(&StateDir::split(volatile, persistent.clone()));
        ephemeral.record_at(&turn(MODEL, Some(2.0)), 0).unwrap();
        ephemeral.record_at(&turn(MODEL, None), 0).unwrap();

        let lifetime = ledger(&StateDir::from_path(persistent)).lifetime().unwrap();

        assert_eq!(lifetime.cost, 2.0);
        assert_eq!(lifetime.ephemeral_cost, 2.0, "{EPHEMERAL_PERSISTS}");
        assert_eq!(lifetime.unpriced_turns, 1, "{UNPRICED_COUNTED}");
    }

    #[test]
    fn an_empty_ledger_summarizes_as_empty_rather_than_as_zero_spend() {
        let (_temp, dir) = state_dir();
        assert!(ledger(&dir).lifetime().unwrap().is_empty());
    }

    #[test]
    fn spend_outlives_the_session_that_produced_it() {
        use crate::sessions::Session;
        use serde_json::Value;

        let (_temp, dir) = state_dir();
        let mut session: Session<Value, Value, Value> = Session::new(MODEL, CWD);
        session.save(&dir).unwrap();
        ledger(&dir).record_at(&turn(MODEL, Some(7.0)), 0).unwrap();

        Session::<Value, Value, Value>::delete(session.id, &dir).unwrap();

        let rows = ledger(&dir).buckets(None).unwrap();
        assert_eq!(rows.len(), 1, "{SURVIVES_DELETE}");
        assert_eq!(rows[0].cost, 7.0, "{SURVIVES_DELETE}");
    }
}
