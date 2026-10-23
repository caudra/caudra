//! What each account of one provider last reported about its quota, shared by
//! every routed provider of that slug and mirrored to the state database so
//! other Caudra processes route on the same picture.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use caudra_storage::StateDir;
use caudra_storage::accounts::{StoredAccountHealth, StoredQuotaWindow, load_account_health};
use tracing::warn;

use super::quota::{QuotaWindow, WindowKind};

/// A newer reading moves the stored row only when a window moved this far.
const PERSIST_STEP_PERCENT: f32 = 1.0;
/// How often a process re-reads what the others wrote.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);
/// How old a reading may be before the router asks the provider again.
pub(crate) const QUOTA_STALE: Duration = Duration::from_secs(10 * 60);

const KIND_FIVE_HOUR: &str = "five_hour";
const KIND_WEEKLY: &str = "weekly";
const KIND_WEEKLY_SCOPED: &str = "weekly_scoped";
const KIND_MONTHLY: &str = "monthly";
const KIND_OTHER: &str = "other";

const SOURCE_RESPONSE: &str = "response";
const SOURCE_USAGE: &str = "usage";

pub(crate) const BLOCKED_RATE_LIMITED: &str = "rate_limited";
pub(crate) const BLOCKED_BILLING: &str = "billing";

/// Where a reading came from. Each source reports its own set of windows,
/// so a reading speaks only for the windows its source reported before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotaSource {
    /// Headers on a model response.
    Response,
    /// The provider's usage endpoint.
    Usage,
}

impl QuotaSource {
    fn stored(self) -> &'static str {
        match self {
            Self::Response => SOURCE_RESPONSE,
            Self::Usage => SOURCE_USAGE,
        }
    }

    fn from_stored(stored: &str) -> Option<Self> {
        match stored {
            SOURCE_RESPONSE => Some(Self::Response),
            SOURCE_USAGE => Some(Self::Usage),
            _ => None,
        }
    }
}

/// One account's latest reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AccountHealth {
    pub windows: Vec<QuotaWindow>,
    /// The source of each window, when known.
    sources: HashMap<WindowKind, QuotaSource>,
    pub blocked_until: Option<u64>,
    pub blocked_reason: Option<String>,
    /// Epoch milliseconds of the reading, 0 when there is none.
    pub observed_at: u64,
}

#[derive(Default)]
struct Entry {
    health: AccountHealth,
    persisted: Option<AccountHealth>,
    last_fetch: Option<Instant>,
    /// The provider answered a usage request with no window at all, so this
    /// process stops asking.
    no_usage: bool,
}

#[derive(Default)]
struct Book {
    entries: HashMap<String, Entry>,
    loaded_at: Option<Instant>,
}

pub(crate) struct HealthBook {
    slug: String,
    book: Mutex<Book>,
}

impl HealthBook {
    pub(crate) fn new(slug: &str) -> Self {
        Self {
            slug: slug.to_owned(),
            book: Mutex::new(Book::default()),
        }
    }

    pub(crate) fn get(&self, account: &str) -> AccountHealth {
        self.book
            .lock()
            .unwrap()
            .entries
            .get(account)
            .map(|entry| entry.health.clone())
            .unwrap_or_default()
    }

    /// Re-reads the shared rows when they are older than the reload interval.
    /// A row another process wrote replaces this one only when it is newer.
    pub(crate) fn reload_if_due(&self, dir: &StateDir) {
        let due = self
            .book
            .lock()
            .unwrap()
            .loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= RELOAD_INTERVAL);
        if !due {
            return;
        }
        let rows = match load_account_health(dir) {
            Ok(rows) => rows,
            Err(error) => {
                warn!(provider = %self.slug, %error, "failed to read account health");
                Vec::new()
            }
        };
        let mut book = self.book.lock().unwrap();
        book.loaded_at = Some(Instant::now());
        for row in rows.into_iter().filter(|row| row.provider == self.slug) {
            let health = from_stored(&row);
            let entry = book.entries.entry(row.account).or_default();
            if health.observed_at > entry.health.observed_at {
                entry.persisted = Some(health.clone());
                entry.health = health;
            }
        }
    }

    /// Takes a reading's windows in place of what its source reported last
    /// time, keeping windows only the other source reports, and lifts a block
    /// the reading shows is over. Returns the row to store when it moved
    /// enough to be worth a write.
    pub(crate) fn observe(
        &self,
        account: &str,
        source: QuotaSource,
        windows: Vec<QuotaWindow>,
        now_ms: u64,
    ) -> Option<StoredAccountHealth> {
        if windows.is_empty() {
            return None;
        }
        let mut book = self.book.lock().unwrap();
        let entry = book.entries.entry(account.to_owned()).or_default();
        let health = &mut entry.health;
        health.windows.retain(|known| {
            windows.iter().any(|window| window.kind == known.kind)
                || health
                    .sources
                    .get(&known.kind)
                    .is_some_and(|known_source| *known_source != source)
        });
        for window in windows {
            health.sources.insert(window.kind.clone(), source);
            match health
                .windows
                .iter_mut()
                .find(|known| known.kind == window.kind)
            {
                Some(known) => *known = window,
                None => health.windows.push(window),
            }
        }
        health
            .sources
            .retain(|kind, _| health.windows.iter().any(|window| window.kind == *kind));
        health.observed_at = now_ms;
        if entry
            .health
            .blocked_until
            .is_some_and(|until| until <= now_ms)
        {
            entry.health.blocked_until = None;
            entry.health.blocked_reason = None;
        }
        self.persist_if_moved(account, entry)
    }

    /// A request went through, so whatever blocked the account is over.
    pub(crate) fn served(&self, account: &str, now_ms: u64) -> Option<StoredAccountHealth> {
        let mut book = self.book.lock().unwrap();
        let entry = book.entries.get_mut(account)?;
        entry.health.blocked_until.take()?;
        entry.health.blocked_reason = None;
        entry.health.observed_at = now_ms;
        self.persist_if_moved(account, entry)
    }

    pub(crate) fn block(
        &self,
        account: &str,
        until_ms: u64,
        reason: &str,
        now_ms: u64,
    ) -> Option<StoredAccountHealth> {
        let mut book = self.book.lock().unwrap();
        let entry = book.entries.entry(account.to_owned()).or_default();
        entry.health.blocked_until = Some(until_ms);
        entry.health.blocked_reason = Some(reason.to_owned());
        entry.health.observed_at = now_ms;
        self.persist_if_moved(account, entry)
    }

    /// Claims a background usage request for the account when its reading is
    /// stale and no request went out recently.
    pub(crate) fn claim_fetch(&self, account: &str, now_ms: u64) -> bool {
        let mut book = self.book.lock().unwrap();
        let entry = book.entries.entry(account.to_owned()).or_default();
        let reading_age = Duration::from_millis(now_ms.saturating_sub(entry.health.observed_at));
        let claimable = !entry.no_usage
            && reading_age >= QUOTA_STALE
            && entry
                .last_fetch
                .is_none_or(|fetched| fetched.elapsed() >= QUOTA_STALE);
        if claimable {
            entry.last_fetch = Some(Instant::now());
        }
        claimable
    }

    pub(crate) fn mark_no_usage(&self, account: &str) {
        self.book
            .lock()
            .unwrap()
            .entries
            .entry(account.to_owned())
            .or_default()
            .no_usage = true;
    }

    fn persist_if_moved(&self, account: &str, entry: &mut Entry) -> Option<StoredAccountHealth> {
        if entry
            .persisted
            .as_ref()
            .is_some_and(|persisted| !moved(persisted, &entry.health))
        {
            return None;
        }
        entry.persisted = Some(entry.health.clone());
        Some(to_stored(&self.slug, account, &entry.health))
    }
}

fn moved(before: &AccountHealth, after: &AccountHealth) -> bool {
    before.blocked_until != after.blocked_until
        || before.windows.len() != after.windows.len()
        || before.sources != after.sources
        || after.windows.iter().any(|window| {
            before
                .windows
                .iter()
                .find(|known| known.kind == window.kind)
                .is_none_or(|known| {
                    (known.used_percent - window.used_percent).abs() >= PERSIST_STEP_PERCENT
                        || known.resets_at != window.resets_at
                })
        })
}

fn to_stored(provider: &str, account: &str, health: &AccountHealth) -> StoredAccountHealth {
    StoredAccountHealth {
        provider: provider.to_owned(),
        account: account.to_owned(),
        windows: health
            .windows
            .iter()
            .map(|window| stored_window(window, health.sources.get(&window.kind).copied()))
            .collect(),
        blocked_until: health.blocked_until,
        blocked_reason: health.blocked_reason.clone(),
        observed_at: health.observed_at,
    }
}

fn from_stored(row: &StoredAccountHealth) -> AccountHealth {
    let mut health = AccountHealth {
        blocked_until: row.blocked_until,
        blocked_reason: row.blocked_reason.clone(),
        observed_at: row.observed_at,
        ..AccountHealth::default()
    };
    for stored in &row.windows {
        let Some(window) = quota_window(stored) else {
            continue;
        };
        if let Some(source) = stored.source.as_deref().and_then(QuotaSource::from_stored) {
            health.sources.insert(window.kind.clone(), source);
        }
        health.windows.push(window);
    }
    health
}

fn stored_window(window: &QuotaWindow, source: Option<QuotaSource>) -> StoredQuotaWindow {
    let (kind, scope) = match &window.kind {
        WindowKind::FiveHour => (KIND_FIVE_HOUR, None),
        WindowKind::Weekly => (KIND_WEEKLY, None),
        WindowKind::WeeklyScoped(scope) => (KIND_WEEKLY_SCOPED, Some(scope.clone())),
        WindowKind::Monthly => (KIND_MONTHLY, None),
        WindowKind::Other(label) => (KIND_OTHER, Some(label.clone())),
    };
    StoredQuotaWindow {
        kind: kind.to_owned(),
        scope,
        used_percent: window.used_percent,
        resets_at: window.resets_at,
        length_secs: window.length_secs,
        source: source.map(|source| source.stored().to_owned()),
    }
}

/// A kind this build does not know is dropped rather than misread.
fn quota_window(stored: &StoredQuotaWindow) -> Option<QuotaWindow> {
    let kind = match (stored.kind.as_str(), stored.scope.clone()) {
        (KIND_FIVE_HOUR, _) => WindowKind::FiveHour,
        (KIND_WEEKLY, _) => WindowKind::Weekly,
        (KIND_WEEKLY_SCOPED, Some(scope)) => WindowKind::WeeklyScoped(scope),
        (KIND_MONTHLY, _) => WindowKind::Monthly,
        (KIND_OTHER, Some(label)) => WindowKind::Other(label),
        _ => return None,
    };
    Some(QuotaWindow {
        kind,
        used_percent: stored.used_percent,
        resets_at: stored.resets_at,
        length_secs: stored.length_secs,
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SLUG: &str = "anthropic";
    const ACCOUNT: &str = "personal";
    const NOW_MS: u64 = 1_800_000_000_000;

    fn window(kind: WindowKind, used: f32) -> QuotaWindow {
        QuotaWindow::new(kind, used, Some(NOW_MS + 60_000))
    }

    fn kinds(book: &HealthBook) -> Vec<WindowKind> {
        book.get(ACCOUNT)
            .windows
            .into_iter()
            .map(|window| window.kind)
            .collect()
    }

    #[test]
    fn first_reading_is_stored() {
        let book = HealthBook::new(SLUG);
        let stored = book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![window(WindowKind::FiveHour, 40.0)],
            NOW_MS,
        );
        assert_eq!(stored.map(|row| row.windows.len()), Some(1));
    }

    #[test_case(40.4, false ; "under_a_point")]
    #[test_case(41.0, true ; "a_point")]
    fn small_moves_are_not_stored(next: f32, expect_write: bool) {
        let book = HealthBook::new(SLUG);
        book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![window(WindowKind::FiveHour, 40.0)],
            NOW_MS,
        );
        let stored = book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![window(WindowKind::FiveHour, next)],
            NOW_MS + 1,
        );
        assert_eq!(stored.is_some(), expect_write);
    }

    #[test]
    fn a_reading_replaces_what_its_source_reported() {
        let book = HealthBook::new(SLUG);
        book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![
                window(WindowKind::FiveHour, 10.0),
                window(WindowKind::Other("secondary".into()), 0.0),
            ],
            NOW_MS,
        );
        let stored = book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![
                window(WindowKind::FiveHour, 10.0),
                window(WindowKind::Weekly, 23.0),
            ],
            NOW_MS + 1,
        );
        assert_eq!(kinds(&book), vec![WindowKind::FiveHour, WindowKind::Weekly]);
        assert!(stored.is_some());
    }

    #[test]
    fn windows_only_the_other_source_reports_stay() {
        let book = HealthBook::new(SLUG);
        book.observe(
            ACCOUNT,
            QuotaSource::Usage,
            vec![
                window(WindowKind::FiveHour, 10.0),
                window(WindowKind::WeeklyScoped("sonnet".into()), 50.0),
            ],
            NOW_MS,
        );
        book.observe(
            ACCOUNT,
            QuotaSource::Response,
            vec![window(WindowKind::FiveHour, 20.0)],
            NOW_MS + 1,
        );
        let health = book.get(ACCOUNT);
        assert_eq!(
            kinds(&book),
            vec![
                WindowKind::FiveHour,
                WindowKind::WeeklyScoped("sonnet".into())
            ]
        );
        assert_eq!(health.windows[0].used_percent, 20.0);
    }

    #[test]
    fn windows_stored_without_a_source_are_replaced() {
        let row = to_stored(
            SLUG,
            ACCOUNT,
            &AccountHealth {
                windows: vec![
                    window(WindowKind::FiveHour, 10.0),
                    window(WindowKind::Other("secondary".into()), 0.0),
                ],
                observed_at: NOW_MS,
                ..AccountHealth::default()
            },
        );
        let book = HealthBook::new(SLUG);
        book.book
            .lock()
            .unwrap()
            .entries
            .entry(ACCOUNT.to_owned())
            .or_default()
            .health = from_stored(&row);
        book.observe(
            ACCOUNT,
            QuotaSource::Usage,
            vec![window(WindowKind::FiveHour, 12.0)],
            NOW_MS + 1,
        );
        assert_eq!(kinds(&book), vec![WindowKind::FiveHour]);
    }

    #[test]
    fn a_served_request_lifts_a_block() {
        let book = HealthBook::new(SLUG);
        book.block(ACCOUNT, NOW_MS + 60_000, BLOCKED_RATE_LIMITED, NOW_MS);
        assert!(book.served(ACCOUNT, NOW_MS + 1).is_some());
        assert_eq!(book.get(ACCOUNT).blocked_until, None);
    }

    #[test]
    fn fetches_are_claimed_once_per_stale_period() {
        let book = HealthBook::new(SLUG);
        assert!(book.claim_fetch(ACCOUNT, NOW_MS));
        assert!(!book.claim_fetch(ACCOUNT, NOW_MS));
    }

    #[test]
    fn accounts_without_usage_are_not_asked_again() {
        let book = HealthBook::new(SLUG);
        book.mark_no_usage(ACCOUNT);
        assert!(!book.claim_fetch(ACCOUNT, NOW_MS));
    }

    #[test_case(WindowKind::FiveHour ; "five_hour")]
    #[test_case(WindowKind::Weekly ; "weekly")]
    #[test_case(WindowKind::WeeklyScoped("opus".into()) ; "scoped")]
    #[test_case(WindowKind::Monthly ; "monthly")]
    #[test_case(WindowKind::Other("2d".into()) ; "other")]
    fn windows_round_trip_through_storage(kind: WindowKind) {
        let book = HealthBook::new(SLUG);
        let row = book
            .observe(
                ACCOUNT,
                QuotaSource::Usage,
                vec![window(kind, 33.0)],
                NOW_MS,
            )
            .unwrap();
        assert_eq!(from_stored(&row), book.get(ACCOUNT));
    }
}
