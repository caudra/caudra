//! Restic-style keep policies for sessions.
//!
//! A session is kept when it matches at least one rule. Calendar rules work on
//! natural boundaries in the caller's time zone and count only periods that
//! contain a session. Duration rules are relative to `now`, not to the newest
//! session. Pinned sessions and sessions with activity in the future are always
//! kept.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, Zoned};
use serde::{Deserialize, Serialize};

use crate::id::CaudraId;

const UNIT_YEARS: char = 'y';
const UNIT_MONTHS: char = 'm';
const UNIT_DAYS: char = 'd';
const UNIT_HOURS: char = 'h';

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DurationParseError {
    #[error("duration is empty; expected a value like 90d or 2y5m7d3h")]
    Empty,
    #[error("duration {input:?} has a number without a unit; units are y, m, d, h")]
    MissingUnit { input: String },
    #[error("duration {input:?} has unit {unit:?} without a number")]
    MissingNumber { input: String, unit: char },
    #[error("duration {input:?} uses unknown unit {unit:?}; units are y, m, d, h")]
    UnknownUnit { input: String, unit: char },
    #[error("duration {input:?} is too large")]
    Overflow { input: String },
}

/// A restic duration such as `90d` or `2y5m7d3h`. Units may appear in any
/// order and repeat; repeated units add up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Duration {
    pub years: u32,
    pub months: u32,
    pub days: u32,
    pub hours: u32,
}

impl Duration {
    pub fn is_zero(self) -> bool {
        self == Self::default()
    }

    /// The instant `self` before `now` in epoch seconds, for callers that
    /// filter on a stored timestamp rather than on [`SessionFacts`].
    pub fn epoch_cutoff(self, now: u64, zone: &TimeZone) -> Option<i64> {
        let cutoff = self.cutoff(&zoned(now, zone)?)?;
        Some(cutoff.timestamp().as_second())
    }

    /// The instant `self` before `now`, or `None` when the calendar
    /// arithmetic leaves the representable range.
    fn cutoff(self, now: &Zoned) -> Option<Zoned> {
        let span = Span::new()
            .try_years(i64::from(self.years))
            .ok()?
            .try_months(i64::from(self.months))
            .ok()?
            .try_days(i64::from(self.days))
            .ok()?
            .try_hours(i64::from(self.hours))
            .ok()?;
        now.checked_sub(span).ok()
    }
}

impl FromStr for Duration {
    type Err = DurationParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let text = input.trim();
        if text.is_empty() {
            return Err(DurationParseError::Empty);
        }
        let overflow = || DurationParseError::Overflow {
            input: input.to_owned(),
        };
        let mut duration = Self::default();
        let mut number: Option<u32> = None;
        for character in text.chars() {
            if let Some(digit) = character.to_digit(10) {
                number = Some(
                    number
                        .unwrap_or(0)
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(digit))
                        .ok_or_else(overflow)?,
                );
                continue;
            }
            let value = number.take().ok_or(DurationParseError::MissingNumber {
                input: input.to_owned(),
                unit: character,
            })?;
            let slot = match character {
                UNIT_YEARS => &mut duration.years,
                UNIT_MONTHS => &mut duration.months,
                UNIT_DAYS => &mut duration.days,
                UNIT_HOURS => &mut duration.hours,
                unit => {
                    return Err(DurationParseError::UnknownUnit {
                        input: input.to_owned(),
                        unit,
                    });
                }
            };
            *slot = slot.checked_add(value).ok_or_else(overflow)?;
        }
        if number.is_some() {
            return Err(DurationParseError::MissingUnit {
                input: input.to_owned(),
            });
        }
        Ok(duration)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return write!(formatter, "0{UNIT_HOURS}");
        }
        for (value, unit) in [
            (self.years, UNIT_YEARS),
            (self.months, UNIT_MONTHS),
            (self.days, UNIT_DAYS),
            (self.hours, UNIT_HOURS),
        ] {
            if value > 0 {
                write!(formatter, "{value}{unit}")?;
            }
        }
        Ok(())
    }
}

impl Serialize for Duration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    /// An empty string is an unset rule, so a config can list every key
    /// with its default and still parse.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Which sessions to keep. Every field is optional and the rules are ORed.
/// Zero counts and zero durations mean the rule is not set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeepPolicy {
    pub keep_last: Option<u32>,
    pub keep_hourly: Option<u32>,
    pub keep_daily: Option<u32>,
    pub keep_weekly: Option<u32>,
    pub keep_monthly: Option<u32>,
    pub keep_yearly: Option<u32>,
    pub keep_within: Option<Duration>,
    pub keep_within_hourly: Option<Duration>,
    pub keep_within_daily: Option<Duration>,
    pub keep_within_weekly: Option<Duration>,
    pub keep_within_monthly: Option<Duration>,
    pub keep_within_yearly: Option<Duration>,
}

impl KeepPolicy {
    /// An empty policy keeps nothing, so callers refuse to act on one.
    pub fn is_empty(&self) -> bool {
        self.keep_last.unwrap_or(0) == 0
            && self.count_rules().into_iter().all(|(_, count)| count == 0)
            && self
                .within_rules()
                .into_iter()
                .all(|(_, duration)| duration.is_zero())
    }

    fn count_rules(&self) -> [(Period, u32); 5] {
        [
            (Period::Hour, self.keep_hourly.unwrap_or(0)),
            (Period::Day, self.keep_daily.unwrap_or(0)),
            (Period::Week, self.keep_weekly.unwrap_or(0)),
            (Period::Month, self.keep_monthly.unwrap_or(0)),
            (Period::Year, self.keep_yearly.unwrap_or(0)),
        ]
    }

    fn within_rules(&self) -> [(Option<Period>, Duration); 6] {
        [
            (None, self.keep_within.unwrap_or_default()),
            (
                Some(Period::Hour),
                self.keep_within_hourly.unwrap_or_default(),
            ),
            (
                Some(Period::Day),
                self.keep_within_daily.unwrap_or_default(),
            ),
            (
                Some(Period::Week),
                self.keep_within_weekly.unwrap_or_default(),
            ),
            (
                Some(Period::Month),
                self.keep_within_monthly.unwrap_or_default(),
            ),
            (
                Some(Period::Year),
                self.keep_within_yearly.unwrap_or_default(),
            ),
        ]
    }
}

impl fmt::Display for KeepPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(last) = self.keep_last.filter(|count| *count > 0) {
            parts.push(format!("keep the last {last}"));
        }
        for (period, count) in self.count_rules() {
            if count > 0 {
                parts.push(format!("keep {count} {}", period.label()));
            }
        }
        for (period, duration) in self.within_rules() {
            if !duration.is_zero() {
                match period {
                    Some(period) => {
                        parts.push(format!("keep {} within {duration}", period.label()));
                    }
                    None => parts.push(format!("keep within {duration}")),
                }
            }
        }
        if parts.is_empty() {
            return formatter.write_str("keep nothing");
        }
        formatter.write_str(&parts.join(", "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// One policy evaluation per session working directory.
    #[default]
    Directory,
    /// One policy evaluation across every session.
    None,
}

impl GroupBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::None => "none",
        }
    }
}

impl FromStr for GroupBy {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "directory" => Ok(Self::Directory),
            "none" | "" => Ok(Self::None),
            other => Err(format!(
                "unknown group-by {other:?}; expected directory or none"
            )),
        }
    }
}

/// The scalar facts a policy needs about one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionFacts {
    pub id: CaudraId,
    pub title: String,
    pub cwd: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_opened_at: Option<u64>,
    pub pinned: bool,
    pub trimmed_at: Option<u64>,
    pub pending_revert: bool,
    pub logical_bytes: u64,
}

impl SessionFacts {
    /// The most recent moment the session was written or opened.
    pub fn active_at(&self) -> u64 {
        self.updated_at.max(self.last_opened_at.unwrap_or(0))
    }

    /// Nothing changed since the last trim, so trimming again does nothing.
    pub fn is_trimmed(&self) -> bool {
        self.trimmed_at
            .is_some_and(|trimmed_at| trimmed_at >= self.updated_at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Period {
    fn label(self) -> &'static str {
        match self {
            Self::Hour => "hourly",
            Self::Day => "daily",
            Self::Week => "weekly",
            Self::Month => "monthly",
            Self::Year => "yearly",
        }
    }

    fn bucket(self, moment: &Zoned) -> (i32, i32, i32, i32) {
        let date = moment.date();
        match self {
            Self::Hour => (
                i32::from(date.year()),
                i32::from(date.month()),
                i32::from(date.day()),
                i32::from(moment.hour()),
            ),
            Self::Day => (
                i32::from(date.year()),
                i32::from(date.month()),
                i32::from(date.day()),
                0,
            ),
            Self::Week => {
                let week = date.iso_week_date();
                (i32::from(week.year()), i32::from(week.week()), 0, 0)
            }
            Self::Month => (i32::from(date.year()), i32::from(date.month()), 0, 0),
            Self::Year => (i32::from(date.year()), 0, 0, 0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "rule")]
pub enum KeepReason {
    Pinned,
    Future,
    Last,
    Period { period: Period },
    Within { duration: Duration },
    PeriodWithin { period: Period, duration: Duration },
}

impl fmt::Display for KeepReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pinned => formatter.write_str("pinned"),
            Self::Future => formatter.write_str("future"),
            Self::Last => formatter.write_str("last"),
            Self::Period { period } => formatter.write_str(period.label()),
            Self::Within { duration } => write!(formatter, "within {duration}"),
            Self::PeriodWithin { period, duration } => {
                write!(formatter, "{} within {duration}", period.label())
            }
        }
    }
}

/// One session and the rules that keep it. No reasons means the policy drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub session: SessionFacts,
    pub reasons: Vec<KeepReason>,
}

impl Decision {
    pub fn keep(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// Sessions sharing one policy evaluation, newest activity first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    pub key: String,
    pub decisions: Vec<Decision>,
}

/// Evaluates `policy` over `sessions`. Groups come back ordered by their newest
/// activity and each group lists its sessions newest first.
pub fn apply(
    policy: &KeepPolicy,
    group_by: GroupBy,
    sessions: Vec<SessionFacts>,
    now: &Zoned,
) -> Vec<Group> {
    let mut grouped: HashMap<String, Vec<SessionFacts>> = HashMap::new();
    for session in sessions {
        let key = match group_by {
            GroupBy::Directory => session.cwd.clone(),
            GroupBy::None => String::new(),
        };
        grouped.entry(key).or_default().push(session);
    }
    let mut groups: Vec<Group> = grouped
        .into_iter()
        .map(|(key, mut sessions)| {
            sessions.sort_by(|left, right| {
                right
                    .active_at()
                    .cmp(&left.active_at())
                    .then_with(|| right.id.as_bytes().cmp(left.id.as_bytes()))
            });
            Group {
                key,
                decisions: decide(policy, sessions, now),
            }
        })
        .collect();
    groups.sort_by(|left, right| {
        let newest = |group: &Group| {
            group
                .decisions
                .first()
                .map_or(0, |decision| decision.session.active_at())
        };
        newest(right)
            .cmp(&newest(left))
            .then_with(|| left.key.cmp(&right.key))
    });
    groups
}

fn decide(policy: &KeepPolicy, sessions: Vec<SessionFacts>, now: &Zoned) -> Vec<Decision> {
    let now_epoch = u64::try_from(now.timestamp().as_second()).unwrap_or(0);
    let zone = now.time_zone().clone();
    let moments: Vec<Option<Zoned>> = sessions
        .iter()
        .map(|session| zoned(session.active_at(), &zone))
        .collect();
    let mut reasons: Vec<Vec<KeepReason>> = sessions
        .iter()
        .map(|session| {
            let mut reasons = Vec::new();
            if session.pinned {
                reasons.push(KeepReason::Pinned);
            }
            if session.active_at() > now_epoch {
                reasons.push(KeepReason::Future);
            }
            reasons
        })
        .collect();

    let mut last = policy.keep_last.unwrap_or(0);
    for reason in &mut reasons {
        if last == 0 {
            break;
        }
        reason.push(KeepReason::Last);
        last -= 1;
    }

    for (period, count) in policy.count_rules() {
        let mut remaining = count;
        let mut previous = None;
        for (index, moment) in moments.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            let Some(moment) = moment else {
                continue;
            };
            let bucket = period.bucket(moment);
            if previous == Some(bucket) {
                continue;
            }
            previous = Some(bucket);
            reasons[index].push(KeepReason::Period { period });
            remaining -= 1;
        }
    }

    for (period, duration) in policy.within_rules() {
        if duration.is_zero() {
            continue;
        }
        let Some(cutoff) = duration.cutoff(now) else {
            continue;
        };
        let mut previous = None;
        for (index, moment) in moments.iter().enumerate() {
            let Some(moment) = moment else {
                continue;
            };
            if *moment < cutoff {
                break;
            }
            match period {
                None => reasons[index].push(KeepReason::Within { duration }),
                Some(period) => {
                    let bucket = period.bucket(moment);
                    if previous == Some(bucket) {
                        continue;
                    }
                    previous = Some(bucket);
                    reasons[index].push(KeepReason::PeriodWithin { period, duration });
                }
            }
        }
    }

    sessions
        .into_iter()
        .zip(reasons)
        .map(|(session, reasons)| Decision { session, reasons })
        .collect()
}

fn zoned(epoch: u64, zone: &TimeZone) -> Option<Zoned> {
    let seconds = i64::try_from(epoch).ok()?;
    Timestamp::from_second(seconds)
        .ok()
        .map(|timestamp| timestamp.to_zoned(zone.clone()))
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use test_case::test_case;

    use super::*;

    const KEPT: &str = "session must be kept";
    const DROPPED: &str = "session must be dropped";
    const REASONS: &str = "keep reasons must match";

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn at_minute(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> u64 {
        let moment = date(year, month, day)
            .at(hour, minute, 0, 0)
            .to_zoned(utc())
            .unwrap();
        u64::try_from(moment.timestamp().as_second()).unwrap()
    }

    fn at(year: i16, month: i8, day: i8, hour: i8) -> u64 {
        at_minute(year, month, day, hour, 0)
    }

    fn facts(index: u8, cwd: &str, active_at: u64) -> SessionFacts {
        SessionFacts {
            id: CaudraId::from_bytes([index; 16]),
            title: format!("session {index}"),
            cwd: cwd.to_owned(),
            created_at: active_at,
            updated_at: active_at,
            last_opened_at: None,
            pinned: false,
            trimmed_at: None,
            pending_revert: false,
            logical_bytes: 0,
        }
    }

    fn now() -> Zoned {
        date(2025, 5, 3).at(12, 0, 0, 0).to_zoned(utc()).unwrap()
    }

    fn decisions(policy: KeepPolicy, sessions: Vec<SessionFacts>) -> Vec<Decision> {
        let groups = apply(&policy, GroupBy::None, sessions, &now());
        assert_eq!(groups.len(), 1);
        groups.into_iter().next().unwrap().decisions
    }

    #[test_case("90d", Duration { days: 90, ..Duration::default() }; "days")]
    #[test_case("2y5m7d3h", Duration { years: 2, months: 5, days: 7, hours: 3 }; "all_units")]
    #[test_case("3h2y", Duration { years: 2, hours: 3, ..Duration::default() }; "any_order")]
    #[test_case("1d1d", Duration { days: 2, ..Duration::default() }; "repeats_add")]
    #[test_case(" 12m ", Duration { months: 12, ..Duration::default() }; "trimmed")]
    fn parses_durations(input: &str, expected: Duration) {
        assert_eq!(input.parse::<Duration>().unwrap(), expected);
        assert_eq!(expected.to_string().parse::<Duration>().unwrap(), expected);
    }

    #[test_case("", DurationParseError::Empty; "empty")]
    #[test_case("7", DurationParseError::MissingUnit { input: "7".into() }; "missing_unit")]
    #[test_case("d", DurationParseError::MissingNumber { input: "d".into(), unit: 'd' }; "missing_number")]
    #[test_case("7w", DurationParseError::UnknownUnit { input: "7w".into(), unit: 'w' }; "unknown_unit")]
    #[test_case("99999999999d", DurationParseError::Overflow { input: "99999999999d".into() }; "overflow")]
    fn rejects_bad_durations(input: &str, expected: DurationParseError) {
        assert_eq!(input.parse::<Duration>().unwrap_err(), expected);
    }

    #[test]
    fn duration_display_round_trips_and_zero_reads_as_hours() {
        assert_eq!(
            Duration {
                years: 1,
                months: 0,
                days: 7,
                hours: 0
            }
            .to_string(),
            "1y7d"
        );
        assert_eq!(Duration::default().to_string(), "0h");
    }

    #[test]
    fn empty_policy_is_detected() {
        assert!(KeepPolicy::default().is_empty());
        assert!(
            KeepPolicy {
                keep_last: Some(0),
                keep_within: Some(Duration::default()),
                ..KeepPolicy::default()
            }
            .is_empty()
        );
        assert!(
            !KeepPolicy {
                keep_daily: Some(1),
                ..KeepPolicy::default()
            }
            .is_empty()
        );
        assert!(
            !KeepPolicy {
                keep_last: Some(1),
                ..KeepPolicy::default()
            }
            .is_empty()
        );
        assert!(
            !KeepPolicy {
                keep_within: Some("1h".parse().unwrap()),
                ..KeepPolicy::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn policy_display_lists_active_rules() {
        let policy = KeepPolicy {
            keep_last: Some(20),
            keep_weekly: Some(4),
            keep_within: Some("90d".parse().unwrap()),
            keep_within_daily: Some("7d".parse().unwrap()),
            ..KeepPolicy::default()
        };
        assert_eq!(
            policy.to_string(),
            "keep the last 20, keep 4 weekly, keep within 90d, keep daily within 7d"
        );
        assert_eq!(KeepPolicy::default().to_string(), "keep nothing");
    }

    #[test]
    fn keep_last_keeps_newest_by_activity() {
        let policy = KeepPolicy {
            keep_last: Some(2),
            ..KeepPolicy::default()
        };
        let mut old_but_opened = facts(1, "/a", at(2025, 1, 1, 9));
        old_but_opened.last_opened_at = Some(at(2025, 5, 2, 9));
        let sessions = vec![
            facts(2, "/a", at(2025, 4, 1, 9)),
            old_but_opened,
            facts(3, "/a", at(2025, 3, 1, 9)),
        ];
        let decisions = decisions(policy, sessions);
        let ids: Vec<u8> = decisions
            .iter()
            .map(|d| d.session.id.as_bytes()[0])
            .collect();
        assert_eq!(ids, [1, 2, 3]);
        assert!(decisions[0].keep(), "{KEPT}");
        assert!(decisions[1].keep(), "{KEPT}");
        assert!(!decisions[2].keep(), "{DROPPED}");
    }

    #[test]
    fn daily_keeps_newest_per_day_that_has_sessions() {
        let policy = KeepPolicy {
            keep_daily: Some(5),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 4, 21, 11)),
            facts(2, "/a", at(2025, 4, 22, 11)),
            facts(3, "/a", at(2025, 4, 23, 11)),
            facts(4, "/a", at(2025, 4, 24, 11)),
            facts(5, "/a", at(2025, 4, 25, 11)),
            facts(6, "/a", at(2025, 4, 25, 23)),
            facts(7, "/a", at(2025, 4, 28, 11)),
            facts(8, "/a", at(2025, 4, 29, 11)),
            facts(9, "/a", at(2025, 5, 1, 11)),
            facts(10, "/a", at(2025, 5, 2, 11)),
            facts(11, "/a", at(2025, 5, 2, 23)),
        ];
        let kept: Vec<u8> = decisions(policy, sessions)
            .into_iter()
            .filter(Decision::keep)
            .map(|d| d.session.id.as_bytes()[0])
            .collect();
        assert_eq!(kept, [11, 9, 8, 7, 6], "{REASONS}");
    }

    fn kept_ids(decisions: &[Decision]) -> Vec<u8> {
        decisions
            .iter()
            .filter(|decision| decision.keep())
            .map(|decision| decision.session.id.as_bytes()[0])
            .collect()
    }

    #[test]
    fn hourly_keeps_the_newest_session_of_each_recent_hour() {
        let policy = KeepPolicy {
            keep_hourly: Some(3),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at_minute(2025, 5, 3, 11, 45)),
            facts(2, "/a", at_minute(2025, 5, 3, 11, 5)),
            facts(3, "/a", at_minute(2025, 5, 3, 10, 30)),
            facts(4, "/a", at_minute(2025, 5, 3, 8, 0)),
            facts(5, "/a", at_minute(2025, 5, 3, 7, 0)),
        ];
        assert_eq!(
            kept_ids(&decisions(policy, sessions)),
            [1, 3, 4],
            "{REASONS}"
        );
    }

    #[test]
    fn within_hourly_keeps_one_session_per_hour_inside_the_cutoff() {
        let duration = "3h".parse::<Duration>().unwrap();
        let policy = KeepPolicy {
            keep_within_hourly: Some(duration),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at_minute(2025, 5, 3, 11, 45)),
            facts(2, "/a", at_minute(2025, 5, 3, 11, 5)),
            facts(3, "/a", at_minute(2025, 5, 3, 9, 30)),
            facts(4, "/a", at_minute(2025, 5, 3, 8, 30)),
        ];
        let decisions = decisions(policy, sessions);
        assert_eq!(kept_ids(&decisions), [1, 3], "{REASONS}");
        assert_eq!(
            decisions[0].reasons,
            [KeepReason::PeriodWithin {
                period: Period::Hour,
                duration
            }]
        );
    }

    /// Sessions 1 and 2 share ISO week 2025-W18, 2 and 3 share April, and
    /// 1 through 4 share 2025, so one fixture exercises every bucket width.
    #[test_case(Period::Week, "1m", vec![1, 3] ; "weekly")]
    #[test_case(Period::Month, "2m", vec![1, 2, 4] ; "monthly")]
    #[test_case(Period::Year, "1y", vec![1, 5] ; "yearly")]
    fn within_period_keeps_one_session_per_bucket_inside_the_cutoff(
        period: Period,
        within: &str,
        expected: Vec<u8>,
    ) {
        let duration = within.parse::<Duration>().unwrap();
        let policy = match period {
            Period::Week => KeepPolicy {
                keep_within_weekly: Some(duration),
                ..KeepPolicy::default()
            },
            Period::Month => KeepPolicy {
                keep_within_monthly: Some(duration),
                ..KeepPolicy::default()
            },
            _ => KeepPolicy {
                keep_within_yearly: Some(duration),
                ..KeepPolicy::default()
            },
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 5, 2, 10)),
            facts(2, "/a", at(2025, 4, 30, 10)),
            facts(3, "/a", at(2025, 4, 20, 10)),
            facts(4, "/a", at(2025, 3, 10, 10)),
            facts(5, "/a", at(2024, 11, 10, 10)),
        ];
        let decisions = decisions(policy, sessions);
        assert_eq!(kept_ids(&decisions), expected, "{REASONS}");
        assert_eq!(
            decisions[0].reasons,
            [KeepReason::PeriodWithin { period, duration }]
        );
    }

    #[test]
    fn within_daily_is_relative_to_now() {
        let policy = KeepPolicy {
            keep_within_daily: Some("7d".parse().unwrap()),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 4, 25, 11)),
            facts(2, "/a", at(2025, 4, 25, 23)),
            facts(3, "/a", at(2025, 4, 28, 11)),
            facts(4, "/a", at(2025, 5, 2, 11)),
            facts(5, "/a", at(2025, 5, 2, 23)),
        ];
        let decisions = decisions(policy, sessions);
        let kept: Vec<u8> = decisions
            .iter()
            .filter(|d| d.keep())
            .map(|d| d.session.id.as_bytes()[0])
            .collect();
        assert_eq!(kept, [5, 3], "{REASONS}");
        assert_eq!(
            decisions[0].reasons,
            [KeepReason::PeriodWithin {
                period: Period::Day,
                duration: "7d".parse().unwrap()
            }]
        );
    }

    #[test]
    fn within_keeps_everything_after_cutoff() {
        let policy = KeepPolicy {
            keep_within: Some("1m".parse().unwrap()),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 4, 3, 12)),
            facts(2, "/a", at(2025, 4, 3, 11)),
            facts(3, "/a", at(2025, 4, 10, 8)),
            facts(4, "/a", at(2025, 4, 10, 9)),
        ];
        let decisions = decisions(policy, sessions);
        let kept: Vec<u8> = decisions
            .iter()
            .filter(|d| d.keep())
            .map(|d| d.session.id.as_bytes()[0])
            .collect();
        assert_eq!(kept, [4, 3, 1], "{REASONS}");
    }

    #[test]
    fn weekly_uses_iso_weeks_and_monthly_yearly_natural_boundaries() {
        let policy = KeepPolicy {
            keep_weekly: Some(2),
            keep_monthly: Some(1),
            keep_yearly: Some(1),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 4, 27, 10)),
            facts(2, "/a", at(2025, 4, 28, 10)),
            facts(3, "/a", at(2025, 5, 2, 10)),
            facts(4, "/a", at(2024, 12, 31, 10)),
        ];
        let decisions = decisions(policy, sessions);
        let by_id = |id: u8| {
            decisions
                .iter()
                .find(|d| d.session.id.as_bytes()[0] == id)
                .unwrap()
        };
        assert_eq!(
            by_id(3).reasons,
            [
                KeepReason::Period {
                    period: Period::Week
                },
                KeepReason::Period {
                    period: Period::Month
                },
                KeepReason::Period {
                    period: Period::Year
                }
            ],
            "{REASONS}"
        );
        assert_eq!(
            by_id(1).reasons,
            [KeepReason::Period {
                period: Period::Week
            }],
            "{REASONS}"
        );
        assert!(!by_id(2).keep(), "{DROPPED}");
        assert!(!by_id(4).keep(), "{DROPPED}");
    }

    #[test]
    fn pinned_and_future_sessions_are_always_kept() {
        let policy = KeepPolicy {
            keep_last: Some(1),
            ..KeepPolicy::default()
        };
        let mut pinned = facts(1, "/a", at(2020, 1, 1, 0));
        pinned.pinned = true;
        let sessions = vec![
            pinned,
            facts(2, "/a", at(2030, 1, 1, 0)),
            facts(3, "/a", at(2025, 5, 1, 0)),
            facts(4, "/a", at(2025, 4, 1, 0)),
        ];
        let decisions = decisions(policy, sessions);
        assert_eq!(decisions[0].reasons, [KeepReason::Future, KeepReason::Last]);
        assert!(!decisions[1].keep(), "{DROPPED}");
        assert!(!decisions[2].keep(), "{DROPPED}");
        assert_eq!(decisions[3].reasons, [KeepReason::Pinned]);
    }

    #[test]
    fn grouping_by_directory_applies_policy_per_directory() {
        let policy = KeepPolicy {
            keep_last: Some(1),
            ..KeepPolicy::default()
        };
        let sessions = vec![
            facts(1, "/a", at(2025, 5, 1, 0)),
            facts(2, "/a", at(2025, 4, 1, 0)),
            facts(3, "/b", at(2025, 3, 1, 0)),
        ];
        let groups = apply(&policy, GroupBy::Directory, sessions.clone(), &now());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "/a");
        assert!(groups[0].decisions[0].keep(), "{KEPT}");
        assert!(!groups[0].decisions[1].keep(), "{DROPPED}");
        assert_eq!(groups[1].key, "/b");
        assert!(groups[1].decisions[0].keep(), "{KEPT}");

        let global = apply(&policy, GroupBy::None, sessions, &now());
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].decisions.iter().filter(|d| d.keep()).count(), 1);
    }

    #[test]
    fn empty_policy_drops_everything_unpinned() {
        let decisions = decisions(
            KeepPolicy::default(),
            vec![facts(1, "/a", at(2025, 5, 1, 0))],
        );
        assert!(!decisions[0].keep(), "{DROPPED}");
    }

    #[test]
    fn trimmed_detection_requires_no_newer_activity() {
        let mut session = facts(1, "/a", at(2025, 5, 1, 0));
        assert!(!session.is_trimmed());
        session.trimmed_at = Some(at(2025, 5, 1, 0));
        assert!(session.is_trimmed());
        session.updated_at = at(2025, 5, 2, 0);
        assert!(!session.is_trimmed());
    }

    #[test_case("directory", GroupBy::Directory)]
    #[test_case("none", GroupBy::None)]
    #[test_case("", GroupBy::None; "empty_means_none")]
    fn parses_group_by(input: &str, expected: GroupBy) {
        assert_eq!(input.parse::<GroupBy>().unwrap(), expected);
    }
}
