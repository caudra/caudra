//! When a schedule trigger is due. Times are unix seconds, and the caller supplies the clock.

use jiff::Timestamp;
use jiff::civil::{Date, Time, Weekday as CivilWeekday};
use jiff::tz::TimeZone;

use crate::meta::{Cadence, CatchUp, Schedule, Weekday};

/// How late an occurrence may be acted on and still count as on time.
pub const ON_TIME_TOLERANCE_S: u64 = 60;
/// Today plus a week of days holds an occurrence on any allowed weekday.
const DAYS_SEARCHED: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Fire for this occurrence.
    Due { scheduled_for: i64, late_by_s: u64 },
    /// Record this missed occurrence as acted on without firing.
    Skip { scheduled_for: i64 },
    /// Nothing is due before `next`.
    Wait { next: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("unknown time zone {name:?}: {reason}")]
    UnknownZone { name: String, reason: String },
}

/// The named IANA zone, or the system zone for `None`.
pub fn resolve_timezone(name: Option<&str>) -> Result<TimeZone, ScheduleError> {
    name.map_or_else(
        || Ok(TimeZone::system()),
        |name| {
            TimeZone::get(name).map_err(|error| ScheduleError::UnknownZone {
                name: name.to_owned(),
                reason: error.to_string(),
            })
        },
    )
}

/// Decides on the latest occurrence after the later of `anchor`, when the automation was
/// armed, and `last`, the last occurrence fired or skipped, that is not after `now`. Missed
/// occurrences collapse into that one: on time within [`ON_TIME_TOLERANCE_S`], it is due;
/// later, `catch_up` decides between firing late and skipping it.
pub fn decide(
    schedule: &Schedule,
    tz: &TimeZone,
    anchor: i64,
    last: Option<i64>,
    now: i64,
) -> Decision {
    let after = last.map_or(anchor, |last| last.max(anchor));
    let Some(scheduled_for) =
        latest(&schedule.cadence, tz, anchor, now).filter(|occurrence| *occurrence > after)
    else {
        return Decision::Wait {
            next: next_after(&schedule.cadence, tz, anchor, after),
        };
    };
    let late_by_s = now.abs_diff(scheduled_for);
    if late_by_s <= ON_TIME_TOLERANCE_S || schedule.catch_up == CatchUp::Once {
        Decision::Due {
            scheduled_for,
            late_by_s,
        }
    } else {
        Decision::Skip { scheduled_for }
    }
}

/// The latest occurrence at or before `now`. An `every` grid starts one period after `anchor`.
fn latest(cadence: &Cadence, tz: &TimeZone, anchor: i64, now: i64) -> Option<i64> {
    match cadence {
        Cadence::Every(every) => {
            let period = period_s(every.as_secs());
            let periods = now.saturating_sub(anchor).div_euclid(period);
            (periods > 0).then(|| anchor.saturating_add(periods.saturating_mul(period)))
        }
        Cadence::At {
            hour,
            minute,
            weekdays,
        } => {
            let time = clock(*hour, *minute)?;
            let mut date = local_date(tz, now)?;
            for _ in 0..DAYS_SEARCHED {
                if let Some(at) = occurrence(tz, date, time, weekdays)
                    && at <= now
                {
                    return Some(at);
                }
                date = date.yesterday().ok()?;
            }
            None
        }
    }
}

/// The first occurrence after `after`, or the end of representable time when there is none.
fn next_after(cadence: &Cadence, tz: &TimeZone, anchor: i64, after: i64) -> i64 {
    match cadence {
        Cadence::Every(every) => {
            let period = period_s(every.as_secs());
            let periods = after
                .saturating_sub(anchor)
                .div_euclid(period)
                .saturating_add(1)
                .max(1);
            anchor.saturating_add(periods.saturating_mul(period))
        }
        Cadence::At {
            hour,
            minute,
            weekdays,
        } => first_at_after(tz, *hour, *minute, weekdays, after)
            .unwrap_or_else(|| Timestamp::MAX.as_second()),
    }
}

fn first_at_after(
    tz: &TimeZone,
    hour: u8,
    minute: u8,
    weekdays: &[Weekday],
    after: i64,
) -> Option<i64> {
    let time = clock(hour, minute)?;
    let mut date = local_date(tz, after)?;
    for _ in 0..DAYS_SEARCHED {
        if let Some(at) = occurrence(tz, date, time, weekdays)
            && at > after
        {
            return Some(at);
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// The instant `time` falls on `date` in `tz`, when `date` is an allowed weekday. A time the
/// clocks skip resolves forward and a time they repeat resolves to its first instant, so each
/// date has exactly one occurrence.
fn occurrence(tz: &TimeZone, date: Date, time: Time, weekdays: &[Weekday]) -> Option<i64> {
    if !weekdays.is_empty() && !weekdays.contains(&weekday(date.weekday())) {
        return None;
    }
    tz.to_ambiguous_timestamp(date.to_datetime(time))
        .compatible()
        .ok()
        .map(Timestamp::as_second)
}

fn clock(hour: u8, minute: u8) -> Option<Time> {
    Time::new(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0).ok()
}

fn local_date(tz: &TimeZone, seconds: i64) -> Option<Date> {
    Some(tz.to_datetime(Timestamp::from_second(seconds).ok()?).date())
}

/// At least a second, so a hand-built zero period cannot divide by zero.
fn period_s(seconds: u64) -> i64 {
    i64::try_from(seconds).unwrap_or(i64::MAX).max(1)
}

fn weekday(day: CivilWeekday) -> Weekday {
    match day {
        CivilWeekday::Monday => Weekday::Mon,
        CivilWeekday::Tuesday => Weekday::Tue,
        CivilWeekday::Wednesday => Weekday::Wed,
        CivilWeekday::Thursday => Weekday::Thu,
        CivilWeekday::Friday => Weekday::Fri,
        CivilWeekday::Saturday => Weekday::Sat,
        CivilWeekday::Sunday => Weekday::Sun,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_case::test_case;

    use super::*;

    const BERLIN: &str = "Europe/Berlin";
    const NEW_YORK: &str = "America/New_York";
    const UTC: &str = "UTC";
    const UNKNOWN_ZONE: &str = "Mars/Olympus_Mons";
    const VALID_TIME: &str = "test instants are valid RFC 3339";
    const KNOWN_ZONE: &str = "the test zones exist in the bundled database";
    const TEN_MINUTES_S: i64 = 600;
    const TEN_MINUTES: Duration = Duration::from_mins(10);
    const DAY_S: i64 = 24 * 60 * 60;
    const MORNING_HOUR: u8 = 9;
    const NIGHT_HOUR: u8 = 2;
    const HALF_PAST: u8 = 30;
    const QUARTER_PAST: u8 = 15;
    const WORKDAYS: [Weekday; 5] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
    ];
    /// A Friday.
    const FRIDAY_MORNING: &str = "2026-10-02T09:00:00Z";
    const SATURDAY_NOON: &str = "2026-10-03T12:00:00Z";
    const MONDAY_MORNING: &str = "2026-10-05T09:00:00Z";
    const THURSDAY_MORNING: &str = "2026-10-01T09:00:00Z";
    const BERLIN_BEFORE_SPRING: &str = "2026-03-28T08:00:00Z";
    const BERLIN_AFTER_SPRING: &str = "2026-03-29T07:00:00Z";
    const BERLIN_SPRING_EVE: &str = "2026-03-28T12:00:00Z";
    const BERLIN_SKIPPED_HALF_PAST_TWO: &str = "2026-03-29T01:30:00Z";
    const BERLIN_SPRING_PREVIOUS: &str = "2026-03-28T01:30:00Z";
    const BERLIN_REPEATED_HALF_PAST_TWO: &str = "2026-10-25T00:30:00Z";
    const BERLIN_SECOND_HALF_PAST_TWO: &str = "2026-10-25T01:30:00Z";
    const BERLIN_AUTUMN_PREVIOUS: &str = "2026-10-24T00:30:00Z";
    const BERLIN_AUTUMN_NEXT: &str = "2026-10-26T01:30:00Z";
    const NEW_YORK_SPRING_PREVIOUS: &str = "2026-03-07T07:15:00Z";
    const NEW_YORK_SKIPPED_QUARTER_PAST_TWO: &str = "2026-03-08T07:15:00Z";
    const NEW_YORK_AUTUMN_PREVIOUS: &str = "2026-10-31T05:30:00Z";
    const NEW_YORK_REPEATED_HALF_PAST_ONE: &str = "2026-11-01T05:30:00Z";
    const NEW_YORK_SECOND_HALF_PAST_ONE: &str = "2026-11-01T06:30:00Z";
    const NEW_YORK_AUTUMN_NEXT: &str = "2026-11-02T06:30:00Z";

    fn unix(instant: &str) -> i64 {
        instant.parse::<Timestamp>().expect(VALID_TIME).as_second()
    }

    fn zone(name: &str) -> TimeZone {
        resolve_timezone(Some(name)).expect(KNOWN_ZONE)
    }

    fn every(period: Duration, catch_up: CatchUp) -> Schedule {
        Schedule {
            cadence: Cadence::Every(period),
            catch_up,
        }
    }

    fn at(hour: u8, minute: u8, weekdays: &[Weekday], catch_up: CatchUp) -> Schedule {
        Schedule {
            cadence: Cadence::At {
                hour,
                minute,
                weekdays: weekdays.to_vec(),
            },
            catch_up,
        }
    }

    fn due(scheduled_for: i64, late_by_s: u64) -> Decision {
        Decision::Due {
            scheduled_for,
            late_by_s,
        }
    }

    #[test_case(TEN_MINUTES_S / 2 => Decision::Wait { next: TEN_MINUTES_S }; "before_the_first_period")]
    #[test_case(TEN_MINUTES_S => due(TEN_MINUTES_S, 0); "on_the_grid")]
    #[test_case(TEN_MINUTES_S + 59 => due(TEN_MINUTES_S, 59); "inside_the_tolerance")]
    #[test_case(TEN_MINUTES_S + 60 => due(TEN_MINUTES_S, 60); "at_the_tolerance")]
    #[test_case(TEN_MINUTES_S + 61 => Decision::Skip { scheduled_for: TEN_MINUTES_S }; "past_the_tolerance")]
    #[test_case(3 * TEN_MINUTES_S + 300 => Decision::Skip { scheduled_for: 3 * TEN_MINUTES_S }; "missed_periods_collapse")]
    fn every_runs_on_a_grid_from_the_anchor(now: i64) -> Decision {
        decide(&every(TEN_MINUTES, CatchUp::Skip), &zone(UTC), 0, None, now)
    }

    #[test]
    fn catch_up_once_fires_the_latest_missed_occurrence_late() {
        assert_eq!(
            decide(
                &every(TEN_MINUTES, CatchUp::Once),
                &zone(UTC),
                0,
                None,
                3 * TEN_MINUTES_S + 300
            ),
            due(3 * TEN_MINUTES_S, 300)
        );
    }

    #[test_case(Some(TEN_MINUTES_S), TEN_MINUTES_S + 300 => Decision::Wait { next: 2 * TEN_MINUTES_S }; "the_occurrence_acted_on")]
    #[test_case(Some(-DAY_S), TEN_MINUTES_S / 2 => Decision::Wait { next: TEN_MINUTES_S }; "a_mark_older_than_the_anchor")]
    #[test_case(None, -TEN_MINUTES_S => Decision::Wait { next: TEN_MINUTES_S }; "a_clock_behind_the_anchor")]
    fn nothing_is_due_after(last: Option<i64>, now: i64) -> Decision {
        decide(&every(TEN_MINUTES, CatchUp::Once), &zone(UTC), 0, last, now)
    }

    #[test_case(FRIDAY_MORNING, SATURDAY_NOON => Decision::Wait { next: unix(MONDAY_MORNING) }; "over_the_weekend")]
    #[test_case(THURSDAY_MORNING, SATURDAY_NOON => Decision::Skip { scheduled_for: unix(FRIDAY_MORNING) }; "a_missed_friday")]
    #[test_case(FRIDAY_MORNING, MONDAY_MORNING => due(unix(MONDAY_MORNING), 0); "on_monday")]
    fn at_fires_only_on_its_weekdays(last: &str, now: &str) -> Decision {
        decide(
            &at(MORNING_HOUR, 0, &WORKDAYS, CatchUp::Skip),
            &zone(UTC),
            unix(THURSDAY_MORNING) - DAY_S,
            Some(unix(last)),
            unix(now),
        )
    }

    #[test]
    fn arming_after_todays_time_waits_for_tomorrow() {
        let anchor = unix(FRIDAY_MORNING) + 1;
        assert_eq!(
            decide(
                &at(MORNING_HOUR, 0, &[], CatchUp::Once),
                &zone(UTC),
                anchor,
                None,
                anchor
            ),
            Decision::Wait {
                next: unix(FRIDAY_MORNING) + DAY_S
            }
        );
    }

    #[test]
    fn at_keeps_the_local_time_across_a_dst_change() {
        assert_eq!(
            decide(
                &at(MORNING_HOUR, 0, &[], CatchUp::Once),
                &zone(BERLIN),
                unix(BERLIN_BEFORE_SPRING) - DAY_S,
                Some(unix(BERLIN_BEFORE_SPRING)),
                unix(BERLIN_SPRING_EVE)
            ),
            Decision::Wait {
                next: unix(BERLIN_AFTER_SPRING)
            }
        );
    }

    #[test_case(BERLIN, NIGHT_HOUR, HALF_PAST, BERLIN_SPRING_PREVIOUS, BERLIN_SKIPPED_HALF_PAST_TWO; "berlin")]
    #[test_case(NEW_YORK, NIGHT_HOUR, QUARTER_PAST, NEW_YORK_SPRING_PREVIOUS, NEW_YORK_SKIPPED_QUARTER_PAST_TWO; "new_york")]
    fn a_skipped_local_time_resolves_forward(
        name: &str,
        hour: u8,
        minute: u8,
        previous: &str,
        resolved: &str,
    ) {
        let schedule = at(hour, minute, &[], CatchUp::Once);
        let tz = zone(name);
        let anchor = unix(previous) - DAY_S;
        assert_eq!(
            decide(
                &schedule,
                &tz,
                anchor,
                Some(unix(previous)),
                unix(previous) + 1
            ),
            Decision::Wait {
                next: unix(resolved)
            }
        );
        assert_eq!(
            decide(&schedule, &tz, anchor, Some(unix(previous)), unix(resolved)),
            due(unix(resolved), 0)
        );
    }

    #[test_case(BERLIN, NIGHT_HOUR, BERLIN_AUTUMN_PREVIOUS, BERLIN_REPEATED_HALF_PAST_TWO, BERLIN_SECOND_HALF_PAST_TWO, BERLIN_AUTUMN_NEXT; "berlin")]
    #[test_case(NEW_YORK, 1, NEW_YORK_AUTUMN_PREVIOUS, NEW_YORK_REPEATED_HALF_PAST_ONE, NEW_YORK_SECOND_HALF_PAST_ONE, NEW_YORK_AUTUMN_NEXT; "new_york")]
    fn a_repeated_local_time_fires_once_at_its_first_instant(
        name: &str,
        hour: u8,
        previous: &str,
        first: &str,
        second: &str,
        next: &str,
    ) {
        let schedule = at(hour, HALF_PAST, &[], CatchUp::Once);
        let tz = zone(name);
        let anchor = unix(previous) - DAY_S;
        assert_eq!(
            decide(&schedule, &tz, anchor, Some(unix(previous)), unix(second)),
            due(unix(first), unix(second).abs_diff(unix(first)))
        );
        assert_eq!(
            decide(&schedule, &tz, anchor, Some(unix(first)), unix(second)),
            Decision::Wait { next: unix(next) }
        );
    }

    #[test]
    fn the_system_zone_needs_no_name() {
        assert_eq!(resolve_timezone(None).ok(), Some(TimeZone::system()));
    }

    #[test]
    fn an_unknown_zone_is_refused_by_name() {
        assert!(matches!(
            resolve_timezone(Some(UNKNOWN_ZONE)),
            Err(ScheduleError::UnknownZone { name, .. }) if name == UNKNOWN_ZONE
        ));
    }
}
