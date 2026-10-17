//! The pure rules that pick an account for a request: switching thresholds,
//! billing eligibility, cache-warm stickiness, and the two strategies. Callers
//! pass what they know about each account and the time; nothing here reads a
//! clock or storage.

use super::quota::{QuotaWindow, WindowKind};
use super::route::{AccountTier, SwitchReason};
use crate::model::Billing;

/// Paced keeps a lineage on its account at a cold point unless another one is
/// this many points further under its schedule.
pub const PACE_MARGIN: f32 = 10.0;
const FULL_PERCENT: f32 = 100.0;
/// Slack this close is a tie.
const SLACK_TIE: f32 = 0.5;
const UNKNOWN_SLACK: f32 = 0.0;
/// The tiers a cold point chooses from, best first.
const OPEN_TIERS: [AccountTier; 2] = [AccountTier::Healthy, AccountTier::Low];
const ALL_TIERS: [AccountTier; 3] = [AccountTier::Healthy, AccountTier::Low, AccountTier::Spent];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Fail over in priority order, and return to the primary after its reset.
    #[default]
    Ordered,
    /// Spread load by schedule, so capacity about to reset is used first.
    Paced,
}

/// Percent left in a watched window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Below this an account is spent, and a lineage leaves it even mid-cache.
    pub switch_below: f32,
    /// Below this a lineage leaves only once its cache is already cold.
    pub prefer_below: f32,
}

/// Why an account cannot take a request at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    SignedOut,
    AuthFailed,
    ModelUnsupported,
}

/// One account of the pool as the router knows it right now.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub name: &'a str,
    pub billing: Billing,
    /// A metered account Auto may move a subscription lineage onto once every
    /// subscription is spent.
    pub fallback: bool,
    pub unusable: Option<Unusable>,
    pub windows: &'a [QuotaWindow],
    /// Epoch milliseconds until which the provider refuses this account.
    pub blocked_until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    Auto,
    Pinned(&'a str),
}

/// Where a conversation lineage stands with its prompt cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lineage<'a> {
    /// No request yet. A child lineage names its session's account.
    New { parent: Option<&'a str> },
    /// The cache on `current` is gone, so a move costs nothing.
    Cold { current: &'a str },
    /// The cache on `current` is warm.
    Warm { current: &'a str },
}

impl<'a> Lineage<'a> {
    fn current(self) -> Option<&'a str> {
        match self {
            Self::New { .. } => None,
            Self::Cold { current } | Self::Warm { current } => Some(current),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyInput<'a> {
    /// In priority order.
    pub candidates: &'a [Candidate<'a>],
    pub selection: Selection<'a>,
    pub strategy: Strategy,
    pub thresholds: Thresholds,
    /// The `switching.windows` names to watch, or every window when `None`.
    pub watched: Option<&'a [&'a str]>,
    pub model_id: &'a str,
    pub lineage: Lineage<'a>,
    pub now_ms: u64,
}

/// Where one account stands for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub tier: AccountTier,
    /// Percent left in the tightest counted window, `None` when none is known.
    pub left: Option<f32>,
    pub tightest: Option<WindowKind>,
    /// Percent used of the tightest window.
    pub used: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// `switch` is set when the lineage moves off another account, and
    /// `exhausted` when the chosen account is spent and the request waits.
    Use {
        index: usize,
        switch: Option<SwitchReason>,
        exhausted: bool,
    },
    NoAccount {
        reason: NoAccountReason,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NoAccountReason {
    Empty,
    PinnedMissing,
    PinnedUnusable(Unusable),
    NothingUsable,
}

/// How far an account runs under its schedule, and when the window that
/// binds it resets.
struct Pace {
    slack: f32,
    resets_at: Option<u64>,
}

/// Unknown windows are optimistic: an account with none counted is healthy.
pub fn assess(
    candidate: &Candidate,
    thresholds: Thresholds,
    watched: Option<&[&str]>,
    model_id: &str,
    now_ms: u64,
) -> Assessment {
    let tightest = counted(candidate.windows, watched, model_id)
        .map(|window| (window, window.left_percent(now_ms)))
        .min_by(|(_, first), (_, second)| first.total_cmp(second));
    let left = tightest.map(|(_, left)| left);
    let tier = match left {
        _ if is_blocked(candidate, now_ms) => AccountTier::Spent,
        Some(left) if left < thresholds.switch_below => AccountTier::Spent,
        Some(left) if left < thresholds.prefer_below => AccountTier::Low,
        _ => AccountTier::Healthy,
    };
    Assessment {
        tier,
        left,
        tightest: tightest.map(|(window, _)| window.kind.clone()),
        used: left.map(|left| FULL_PERCENT - left),
    }
}

pub fn decide(input: &PolicyInput) -> Decision {
    if input.candidates.is_empty() {
        return no_account(NoAccountReason::Empty);
    }
    let pool = Pool::new(input);
    match input.selection {
        Selection::Pinned(name) => pool.pinned(name),
        Selection::Auto => pool.auto(),
    }
}

/// Other usable accounts the user could move to from `current`, best first,
/// for the exhaustion card. Metered accounts are offered whatever their
/// `fallback`, since the user decides.
pub fn alternatives(input: &PolicyInput, current: usize) -> Vec<usize> {
    let pool = Pool::new(input);
    let others: Vec<usize> = pool
        .usable_indices()
        .filter(|index| *index != current)
        .collect();
    ALL_TIERS
        .into_iter()
        .flat_map(|tier| pool.ranked(pool.of_tier(&others, tier)))
        .collect()
}

fn counted<'w>(
    windows: &'w [QuotaWindow],
    watched: Option<&[&str]>,
    model_id: &str,
) -> impl Iterator<Item = &'w QuotaWindow> {
    windows.iter().filter(move |window| {
        window.kind.applies_to(model_id)
            && watched.is_none_or(|names| {
                window
                    .kind
                    .config_name()
                    .is_some_and(|name| names.contains(&name))
            })
    })
}

fn is_blocked(candidate: &Candidate, now_ms: u64) -> bool {
    candidate.blocked_until.is_some_and(|until| until > now_ms)
}

fn no_account(reason: NoAccountReason) -> Decision {
    Decision::NoAccount { reason }
}

struct Pool<'a> {
    input: &'a PolicyInput<'a>,
    assessments: Vec<Assessment>,
}

impl<'a> Pool<'a> {
    fn new(input: &'a PolicyInput<'a>) -> Self {
        let assessments = input
            .candidates
            .iter()
            .map(|candidate| {
                assess(
                    candidate,
                    input.thresholds,
                    input.watched,
                    input.model_id,
                    input.now_ms,
                )
            })
            .collect();
        Self { input, assessments }
    }

    fn pinned(&self, name: &str) -> Decision {
        let Some(index) = self.position(name) else {
            return no_account(NoAccountReason::PinnedMissing);
        };
        if let Some(unusable) = self.input.candidates[index].unusable {
            return no_account(NoAccountReason::PinnedUnusable(unusable));
        }
        let moved = self
            .input
            .lineage
            .current()
            .is_some_and(|current| current != name);
        Decision::Use {
            index,
            switch: moved.then_some(SwitchReason::Selected),
            exhausted: self.tier(index) == AccountTier::Spent,
        }
    }

    fn auto(&self) -> Decision {
        let eligible = self.eligible();
        let current = self
            .input
            .lineage
            .current()
            .and_then(|name| self.position(name));
        let choice = match self.input.lineage {
            Lineage::Warm { .. } => {
                let open: Vec<usize> = eligible
                    .iter()
                    .copied()
                    .filter(|index| self.tier(*index) != AccountTier::Spent)
                    .collect();
                current
                    .filter(|index| open.contains(index))
                    .or_else(|| self.best(&open))
            }
            Lineage::Cold { .. } => self.cold_choice(&eligible, current),
            Lineage::New { parent } => parent
                .and_then(|name| self.position(name))
                .filter(|index| {
                    eligible.contains(index) && self.tier(*index) == AccountTier::Healthy
                })
                .or_else(|| self.cold_choice(&eligible, None)),
        };
        match choice {
            Some(index) => self.chosen(index, current, false),
            None => self.exhausted(&eligible, current),
        }
    }

    /// The best healthy account, else the best low one. The current account
    /// stays when it is low among low ones, or within the pace margin.
    fn cold_choice(&self, eligible: &[usize], current: Option<usize>) -> Option<usize> {
        OPEN_TIERS.into_iter().find_map(|tier| {
            let group = self.of_tier(eligible, tier);
            let best = self.best(&group)?;
            let kept = current.filter(|current| {
                group.contains(current)
                    && (tier == AccountTier::Low || self.within_pace_margin(*current, best))
            });
            Some(kept.unwrap_or(best))
        })
    }

    /// Every eligible account is spent: wait on the current one, else on the
    /// strategy's best.
    fn exhausted(&self, eligible: &[usize], current: Option<usize>) -> Decision {
        match current
            .filter(|index| self.usable(*index))
            .or_else(|| self.best(eligible))
        {
            Some(index) => self.chosen(index, current, true),
            None => no_account(NoAccountReason::NothingUsable),
        }
    }

    fn chosen(&self, index: usize, current: Option<usize>, exhausted: bool) -> Decision {
        let moved = self.input.lineage.current().is_some() && current != Some(index);
        Decision::Use {
            index,
            switch: moved.then(|| self.leave_reason(current)),
            exhausted,
        }
    }

    /// A current account that is no longer in the pool counts as signed out.
    fn leave_reason(&self, current: Option<usize>) -> SwitchReason {
        let Some(index) = current else {
            return SwitchReason::AuthFailed;
        };
        let candidate = &self.input.candidates[index];
        match candidate.unusable {
            Some(Unusable::ModelUnsupported) => return SwitchReason::ModelUnsupported,
            Some(Unusable::SignedOut | Unusable::AuthFailed) => return SwitchReason::AuthFailed,
            None if is_blocked(candidate, self.input.now_ms) => {
                return SwitchReason::RateLimited {
                    until_ms: candidate.blocked_until,
                };
            }
            None => {}
        }
        let assessment = &self.assessments[index];
        let (Some(window), Some(used)) = (assessment.tightest.clone(), assessment.used) else {
            return SwitchReason::Rebalanced;
        };
        let used_percent = used.round() as u8;
        match assessment.tier {
            AccountTier::Spent => SwitchReason::Spent {
                window,
                used_percent,
            },
            AccountTier::Low => SwitchReason::Low {
                window,
                used_percent,
            },
            AccountTier::Healthy => SwitchReason::Rebalanced,
        }
    }

    /// Accounts of the lineage's billing kind. A metered lineage may move to
    /// anything; a subscription one reaches a metered `fallback` only once
    /// every usable subscription is spent.
    fn eligible(&self) -> Vec<usize> {
        let candidates = self.input.candidates;
        let anchor = match self.input.lineage {
            Lineage::New { parent } => parent,
            Lineage::Cold { current } | Lineage::Warm { current } => Some(current),
        };
        let on_subscription = anchor
            .and_then(|name| self.position(name))
            .or_else(|| self.usable_indices().next())
            .is_some_and(|index| candidates[index].billing.is_subscription());
        if !on_subscription {
            return self.usable_indices().collect();
        }
        let subscriptions_spent = self
            .usable_indices()
            .filter(|index| candidates[*index].billing.is_subscription())
            .all(|index| self.tier(index) == AccountTier::Spent);
        self.usable_indices()
            .filter(|index| {
                let candidate = &candidates[*index];
                candidate.billing.is_subscription() || (subscriptions_spent && candidate.fallback)
            })
            .collect()
    }

    /// `group` is in priority order.
    fn best(&self, group: &[usize]) -> Option<usize> {
        match self.input.strategy {
            Strategy::Ordered => group.first().copied(),
            Strategy::Paced => group.iter().copied().reduce(|best, next| {
                if self.paces_ahead(next, best) {
                    next
                } else {
                    best
                }
            }),
        }
    }

    fn ranked(&self, mut group: Vec<usize>) -> Vec<usize> {
        let mut ranked = Vec::with_capacity(group.len());
        while let Some(best) = self.best(&group) {
            group.retain(|index| *index != best);
            ranked.push(best);
        }
        ranked
    }

    /// More slack wins; a tie goes to the earlier reset, then to priority.
    fn paces_ahead(&self, next: usize, best: usize) -> bool {
        let (next, best) = (self.pace(next), self.pace(best));
        if (next.slack - best.slack).abs() >= SLACK_TIE {
            return next.slack > best.slack;
        }
        match (next.resets_at, best.resets_at) {
            (Some(next), Some(best)) => next < best,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    fn within_pace_margin(&self, current: usize, best: usize) -> bool {
        self.input.strategy == Strategy::Paced
            && self.pace(best).slack - self.pace(current).slack < PACE_MARGIN
    }

    /// The minimum of `elapsed% - used%` over counted windows whose schedule
    /// is known. A window past its reset has no known next reset.
    fn pace(&self, index: usize) -> Pace {
        let input = self.input;
        counted(
            input.candidates[index].windows,
            input.watched,
            input.model_id,
        )
        .filter_map(|window| {
            let elapsed = window.elapsed_percent(input.now_ms)?;
            Some(Pace {
                slack: elapsed - (FULL_PERCENT - window.left_percent(input.now_ms)),
                resets_at: window.resets_at.filter(|reset| *reset > input.now_ms),
            })
        })
        .min_by(|first, second| first.slack.total_cmp(&second.slack))
        .unwrap_or(Pace {
            slack: UNKNOWN_SLACK,
            resets_at: None,
        })
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.input
            .candidates
            .iter()
            .position(|candidate| candidate.name == name)
    }

    fn usable(&self, index: usize) -> bool {
        self.input.candidates[index].unusable.is_none()
    }

    fn usable_indices(&self) -> impl Iterator<Item = usize> {
        (0..self.input.candidates.len()).filter(|index| self.usable(*index))
    }

    fn tier(&self, index: usize) -> AccountTier {
        self.assessments[index].tier
    }

    fn of_tier(&self, group: &[usize], tier: AccountTier) -> Vec<usize> {
        group
            .iter()
            .copied()
            .filter(|index| self.tier(*index) == tier)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::accounts::quota::{CONFIG_FIVE_HOUR, CONFIG_MONTHLY, CONFIG_WEEKLY};

    const NOW_MS: u64 = 1_800_000_000_000;
    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    const BLOCKED_UNTIL: u64 = NOW_MS + HOUR_MS;
    const SONNET: &str = "claude-sonnet-5";
    const OPUS: &str = "claude-opus-5-5";
    const OPUS_SCOPE: &str = "opus";
    const OTHER_WINDOW: &str = "2d";
    const PRIMARY: &str = "personal";
    const SECONDARY: &str = "work";
    const SPARE: &str = "spare";
    const METERED: &str = "api";
    const RETIRED: &str = "retired";
    const MISSING: &str = "gone";
    const THRESHOLDS: Thresholds = Thresholds {
        switch_below: 5.0,
        prefer_below: 20.0,
    };
    const WATCH_FIVE_HOUR: &[&str] = &[CONFIG_FIVE_HOUR];
    const WATCH_WEEKLY: &[&str] = &[CONFIG_WEEKLY];
    const WATCH_NAMED: &[&str] = &[CONFIG_FIVE_HOUR, CONFIG_WEEKLY, CONFIG_MONTHLY];

    struct Account {
        name: &'static str,
        billing: Billing,
        fallback: bool,
        unusable: Option<Unusable>,
        windows: Vec<QuotaWindow>,
        blocked_until: Option<u64>,
    }

    impl Account {
        fn with(mut self, window: QuotaWindow) -> Self {
            self.windows.push(window);
            self
        }

        fn fallback(mut self) -> Self {
            self.fallback = true;
            self
        }

        fn blocked(mut self) -> Self {
            self.blocked_until = Some(BLOCKED_UNTIL);
            self
        }

        fn unusable(mut self, reason: Unusable) -> Self {
            self.unusable = Some(reason);
            self
        }

        fn candidate(&self) -> Candidate<'_> {
            Candidate {
                name: self.name,
                billing: self.billing,
                fallback: self.fallback,
                unusable: self.unusable,
                windows: &self.windows,
                blocked_until: self.blocked_until,
            }
        }
    }

    fn account(name: &'static str, billing: Billing) -> Account {
        Account {
            name,
            billing,
            fallback: false,
            unusable: None,
            windows: Vec::new(),
            blocked_until: None,
        }
    }

    fn sub(name: &'static str) -> Account {
        account(name, Billing::Subscription)
    }

    fn metered(name: &'static str) -> Account {
        account(name, Billing::Api)
    }

    /// A five-hour window `hours_left` before its reset, so its elapsed share
    /// is `(5 - hours_left) * 20`.
    fn paced(used: f32, hours_left: u64) -> QuotaWindow {
        QuotaWindow::new(
            WindowKind::FiveHour,
            used,
            Some(NOW_MS + hours_left * HOUR_MS),
        )
    }

    fn five_hour(used: f32) -> QuotaWindow {
        paced(used, 1)
    }

    fn refilled(used: f32) -> QuotaWindow {
        QuotaWindow::new(WindowKind::FiveHour, used, Some(NOW_MS - 1))
    }

    fn weekly(used: f32) -> QuotaWindow {
        QuotaWindow::new(WindowKind::Weekly, used, Some(NOW_MS + DAY_MS))
    }

    fn opus_weekly(used: f32) -> QuotaWindow {
        QuotaWindow::new(
            WindowKind::WeeklyScoped(OPUS_SCOPE.to_owned()),
            used,
            Some(NOW_MS + DAY_MS),
        )
    }

    fn other(used: f32) -> QuotaWindow {
        QuotaWindow::new(WindowKind::Other(OTHER_WINDOW.to_owned()), used, None)
    }

    fn input<'a>(
        candidates: &'a [Candidate<'a>],
        selection: Selection<'a>,
        strategy: Strategy,
        lineage: Lineage<'a>,
    ) -> PolicyInput<'a> {
        PolicyInput {
            candidates,
            selection,
            strategy,
            thresholds: THRESHOLDS,
            watched: None,
            model_id: SONNET,
            lineage,
            now_ms: NOW_MS,
        }
    }

    fn run(
        accounts: &[Account],
        selection: Selection,
        strategy: Strategy,
        lineage: Lineage,
    ) -> Decision {
        let candidates: Vec<Candidate> = accounts.iter().map(Account::candidate).collect();
        decide(&input(&candidates, selection, strategy, lineage))
    }

    fn chose(index: usize, switch: Option<SwitchReason>) -> Decision {
        Decision::Use {
            index,
            switch,
            exhausted: false,
        }
    }

    fn waits(index: usize, switch: Option<SwitchReason>) -> Decision {
        Decision::Use {
            index,
            switch,
            exhausted: true,
        }
    }

    fn spent(used_percent: u8) -> Option<SwitchReason> {
        Some(SwitchReason::Spent {
            window: WindowKind::FiveHour,
            used_percent,
        })
    }

    fn low(used_percent: u8) -> Option<SwitchReason> {
        Some(SwitchReason::Low {
            window: WindowKind::FiveHour,
            used_percent,
        })
    }

    #[test_case(vec![], None, None, SONNET, (AccountTier::Healthy, None, None) ; "unknown_is_healthy")]
    #[test_case(vec![five_hour(50.0)], None, None, SONNET, (AccountTier::Healthy, Some(50.0), Some(WindowKind::FiveHour)) ; "half_used_is_healthy")]
    #[test_case(vec![five_hour(80.0)], None, None, SONNET, (AccountTier::Healthy, Some(20.0), Some(WindowKind::FiveHour)) ; "at_prefer_below_is_healthy")]
    #[test_case(vec![five_hour(85.0)], None, None, SONNET, (AccountTier::Low, Some(15.0), Some(WindowKind::FiveHour)) ; "under_prefer_below_is_low")]
    #[test_case(vec![five_hour(95.0)], None, None, SONNET, (AccountTier::Low, Some(5.0), Some(WindowKind::FiveHour)) ; "at_switch_below_is_low")]
    #[test_case(vec![five_hour(97.0)], None, None, SONNET, (AccountTier::Spent, Some(3.0), Some(WindowKind::FiveHour)) ; "under_switch_below_is_spent")]
    #[test_case(vec![five_hour(30.0), weekly(90.0)], None, None, SONNET, (AccountTier::Low, Some(10.0), Some(WindowKind::Weekly)) ; "tightest_window_decides")]
    #[test_case(vec![], Some(BLOCKED_UNTIL), None, SONNET, (AccountTier::Spent, None, None) ; "blocked_is_spent")]
    #[test_case(vec![], Some(NOW_MS), None, SONNET, (AccountTier::Healthy, None, None) ; "lifted_block_is_healthy")]
    #[test_case(vec![refilled(99.0)], None, None, SONNET, (AccountTier::Healthy, Some(100.0), Some(WindowKind::FiveHour)) ; "reset_window_is_full")]
    #[test_case(vec![opus_weekly(99.0)], None, None, SONNET, (AccountTier::Healthy, None, None) ; "scoped_window_skips_other_models")]
    #[test_case(vec![opus_weekly(99.0)], None, None, OPUS, (AccountTier::Spent, Some(1.0), Some(WindowKind::WeeklyScoped(OPUS_SCOPE.to_owned()))) ; "scoped_window_limits_its_model")]
    #[test_case(vec![weekly(99.0)], None, Some(WATCH_FIVE_HOUR), SONNET, (AccountTier::Healthy, None, None) ; "unwatched_window_is_ignored")]
    #[test_case(vec![opus_weekly(99.0)], None, Some(WATCH_WEEKLY), OPUS, (AccountTier::Spent, Some(1.0), Some(WindowKind::WeeklyScoped(OPUS_SCOPE.to_owned()))) ; "scoped_window_answers_to_weekly")]
    #[test_case(vec![other(99.0)], None, None, SONNET, (AccountTier::Spent, Some(1.0), Some(WindowKind::Other(OTHER_WINDOW.to_owned()))) ; "other_window_counts_when_watching_all")]
    #[test_case(vec![other(99.0)], None, Some(WATCH_NAMED), SONNET, (AccountTier::Healthy, None, None) ; "other_window_skipped_when_watching_names")]
    fn assess_places_accounts_in_tiers(
        windows: Vec<QuotaWindow>,
        blocked_until: Option<u64>,
        watched: Option<&[&str]>,
        model_id: &str,
        expected: (AccountTier, Option<f32>, Option<WindowKind>),
    ) {
        let mut account = sub(PRIMARY);
        account.windows = windows;
        account.blocked_until = blocked_until;
        let assessment = assess(&account.candidate(), THRESHOLDS, watched, model_id, NOW_MS);
        assert_eq!(
            (assessment.tier, assessment.left, assessment.tightest),
            expected
        );
    }

    #[test]
    fn assess_reports_the_tightest_share_used() {
        let account = sub(PRIMARY).with(five_hour(30.0)).with(weekly(85.0));
        let assessment = assess(&account.candidate(), THRESHOLDS, None, SONNET, NOW_MS);
        assert_eq!(assessment.used, Some(85.0));
    }

    #[test_case(vec![sub(PRIMARY).with(five_hour(85.0)), sub(SECONDARY)], Lineage::Warm { current: PRIMARY }, chose(0, None) ; "warm_stays_on_a_low_account")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY)], Lineage::Warm { current: PRIMARY }, chose(1, spent(97)) ; "warm_leaves_a_spent_account")]
    #[test_case(vec![sub(PRIMARY).blocked(), sub(SECONDARY)], Lineage::Warm { current: PRIMARY }, chose(1, Some(SwitchReason::RateLimited { until_ms: Some(BLOCKED_UNTIL) })) ; "warm_leaves_a_blocked_account")]
    #[test_case(vec![sub(PRIMARY).unusable(Unusable::SignedOut), sub(SECONDARY)], Lineage::Warm { current: PRIMARY }, chose(1, Some(SwitchReason::AuthFailed)) ; "warm_leaves_a_signed_out_account")]
    #[test_case(vec![sub(PRIMARY).unusable(Unusable::ModelUnsupported), sub(SECONDARY)], Lineage::Warm { current: PRIMARY }, chose(1, Some(SwitchReason::ModelUnsupported)) ; "warm_leaves_an_unsupported_model")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], Lineage::Warm { current: MISSING }, chose(0, Some(SwitchReason::AuthFailed)) ; "warm_leaves_a_removed_account")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(85.0)), sub(SPARE)], Lineage::Warm { current: PRIMARY }, chose(1, spent(97)) ; "warm_takes_the_first_open_account")]
    #[test_case(vec![sub(PRIMARY).with(refilled(99.0)), sub(SECONDARY)], Lineage::Warm { current: SECONDARY }, chose(1, None) ; "warm_stays_off_the_primary_after_its_reset")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], Lineage::Cold { current: PRIMARY }, chose(0, None) ; "cold_stays_on_the_primary")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(85.0)), sub(SECONDARY)], Lineage::Cold { current: PRIMARY }, chose(1, low(85)) ; "cold_leaves_a_low_account")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(85.0)), sub(SECONDARY).with(five_hour(90.0))], Lineage::Cold { current: SECONDARY }, chose(1, None) ; "cold_keeps_low_when_all_are_low")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(90.0))], Lineage::Cold { current: PRIMARY }, chose(1, spent(97)) ; "cold_spent_takes_a_low_account")]
    #[test_case(vec![sub(PRIMARY).with(refilled(99.0)), sub(SECONDARY)], Lineage::Cold { current: SECONDARY }, chose(0, Some(SwitchReason::Rebalanced)) ; "cold_returns_to_the_primary_after_its_reset")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], Lineage::New { parent: None }, chose(0, None) ; "new_lineage_takes_the_primary")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], Lineage::New { parent: Some(SECONDARY) }, chose(1, None) ; "child_inherits_a_healthy_parent")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY).with(five_hour(85.0))], Lineage::New { parent: Some(SECONDARY) }, chose(0, None) ; "child_ignores_a_low_parent")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), metered(METERED)], Lineage::Warm { current: PRIMARY }, waits(0, None) ; "metered_needs_fallback")]
    #[test_case(vec![metered(METERED).fallback(), sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(85.0))], Lineage::Warm { current: PRIMARY }, chose(2, spent(97)) ; "fallback_waits_for_every_subscription")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(99.0)), metered(METERED).fallback()], Lineage::Warm { current: PRIMARY }, chose(2, spent(97)) ; "fallback_takes_over_when_every_subscription_is_spent")]
    #[test_case(vec![sub(PRIMARY), metered(METERED).fallback()], Lineage::Warm { current: METERED }, chose(1, None) ; "metered_lineage_stays_while_warm")]
    #[test_case(vec![sub(PRIMARY), metered(METERED).fallback()], Lineage::Cold { current: METERED }, chose(0, Some(SwitchReason::Rebalanced)) ; "metered_lineage_returns_to_a_subscription_when_cold")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(99.0))], Lineage::Warm { current: SECONDARY }, waits(1, None) ; "warm_all_spent_stays_and_waits")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(99.0))], Lineage::Cold { current: SECONDARY }, waits(1, None) ; "cold_all_spent_stays_and_waits")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(99.0))], Lineage::New { parent: None }, waits(0, None) ; "new_lineage_waits_on_the_first_spent_account")]
    #[test_case(vec![sub(PRIMARY).unusable(Unusable::SignedOut), sub(SECONDARY).with(five_hour(99.0))], Lineage::Warm { current: PRIMARY }, waits(1, Some(SwitchReason::AuthFailed)) ; "signed_out_moves_to_a_spent_account")]
    #[test_case(vec![sub(PRIMARY).unusable(Unusable::AuthFailed)], Lineage::Warm { current: PRIMARY }, Decision::NoAccount { reason: NoAccountReason::NothingUsable } ; "nothing_usable")]
    #[test_case(vec![], Lineage::New { parent: None }, Decision::NoAccount { reason: NoAccountReason::Empty } ; "no_accounts")]
    fn auto_ordered_decides(accounts: Vec<Account>, lineage: Lineage<'static>, expected: Decision) {
        assert_eq!(
            run(&accounts, Selection::Auto, Strategy::Ordered, lineage),
            expected
        );
    }

    #[test_case(vec![sub(PRIMARY).with(paced(10.0, 4)), sub(SECONDARY).with(paced(30.0, 1))], Lineage::New { parent: None }, chose(1, None) ; "most_slack_wins")]
    #[test_case(vec![sub(PRIMARY).with(paced(10.0, 4)), sub(SECONDARY).with(paced(65.0, 1))], Lineage::Cold { current: PRIMARY }, chose(0, None) ; "margin_keeps_the_current_account")]
    #[test_case(vec![sub(PRIMARY).with(paced(10.0, 4)), sub(SECONDARY).with(paced(30.0, 1))], Lineage::Cold { current: PRIMARY }, chose(1, Some(SwitchReason::Rebalanced)) ; "beyond_the_margin_rebalances")]
    #[test_case(vec![sub(PRIMARY).with(paced(0.0, 5)), sub(SECONDARY).with(paced(80.0, 1))], Lineage::New { parent: None }, chose(1, None) ; "tie_goes_to_the_earliest_reset")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], Lineage::New { parent: None }, chose(0, None) ; "full_tie_goes_to_priority")]
    #[test_case(vec![sub(PRIMARY).with(paced(10.0, 4)), sub(SECONDARY).with(paced(30.0, 1))], Lineage::Warm { current: PRIMARY }, chose(0, None) ; "warm_ignores_slack")]
    #[test_case(vec![sub(PRIMARY).with(paced(97.0, 4)), sub(SECONDARY).with(paced(10.0, 4)), sub(SPARE).with(paced(30.0, 1))], Lineage::Warm { current: PRIMARY }, chose(2, spent(97)) ; "warm_spent_takes_the_most_slack")]
    #[test_case(vec![sub(PRIMARY).with(paced(90.0, 1)), sub(SECONDARY).with(weekly(81.0))], Lineage::Cold { current: PRIMARY }, chose(0, None) ; "cold_keeps_low_whatever_the_slack")]
    fn auto_paced_decides(accounts: Vec<Account>, lineage: Lineage<'static>, expected: Decision) {
        assert_eq!(
            run(&accounts, Selection::Auto, Strategy::Paced, lineage),
            expected
        );
    }

    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], SECONDARY, Lineage::Warm { current: SECONDARY }, chose(1, None) ; "pinned_account_serves")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], SECONDARY, Lineage::Warm { current: PRIMARY }, chose(1, Some(SwitchReason::Selected)) ; "pinning_moves_the_lineage")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY)], SECONDARY, Lineage::New { parent: Some(PRIMARY) }, chose(1, None) ; "new_lineage_is_not_a_move")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY).with(five_hour(99.0))], SECONDARY, Lineage::Warm { current: SECONDARY }, waits(1, None) ; "spent_pinned_account_waits")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), metered(METERED)], METERED, Lineage::Warm { current: PRIMARY }, chose(1, Some(SwitchReason::Selected)) ; "pinned_metered_ignores_fallback")]
    #[test_case(vec![sub(PRIMARY)], MISSING, Lineage::Warm { current: PRIMARY }, Decision::NoAccount { reason: NoAccountReason::PinnedMissing } ; "pinned_account_missing")]
    #[test_case(vec![sub(PRIMARY), sub(SECONDARY).unusable(Unusable::SignedOut)], SECONDARY, Lineage::Warm { current: PRIMARY }, Decision::NoAccount { reason: NoAccountReason::PinnedUnusable(Unusable::SignedOut) } ; "pinned_account_signed_out")]
    fn pinned_decides(
        accounts: Vec<Account>,
        pinned: &'static str,
        lineage: Lineage<'static>,
        expected: Decision,
    ) {
        assert_eq!(
            run(
                &accounts,
                Selection::Pinned(pinned),
                Strategy::Ordered,
                lineage
            ),
            expected
        );
    }

    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(five_hour(85.0)), sub(SPARE).with(five_hour(99.0)), metered(METERED), sub(RETIRED).unusable(Unusable::SignedOut)], Strategy::Ordered, vec![3, 1, 2] ; "ordered_by_tier_then_priority")]
    #[test_case(vec![sub(PRIMARY).with(five_hour(97.0)), sub(SECONDARY).with(paced(10.0, 4)), sub(SPARE).with(paced(30.0, 1)), metered(METERED)], Strategy::Paced, vec![2, 1, 3] ; "paced_by_tier_then_slack")]
    fn alternatives_rank_the_rest(
        accounts: Vec<Account>,
        strategy: Strategy,
        expected: Vec<usize>,
    ) {
        let candidates: Vec<Candidate> = accounts.iter().map(Account::candidate).collect();
        let lineage = Lineage::Warm { current: PRIMARY };
        assert_eq!(
            alternatives(&input(&candidates, Selection::Auto, strategy, lineage), 0),
            expected
        );
    }
}
