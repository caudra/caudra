//! What the run is worth saying once the TUI is gone. Rendered with plain SGR
//! rather than theme colors: the alternate screen is already down, so the
//! terminal's own background is back and an accent picked for the TUI palette
//! has no contrast guarantee against it.

use std::fmt::Write;
use std::time::Duration;

use caudra_providers::TokenUsage;
use caudra_storage::id::CaudraId;

use crate::AppSession;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Half-block wordmark. The lone ascender sits over `d`'s right column.
const WORDMARK: &[&str] = &[
    "                      ▄",
    "    █▀▀▀ ▄▀▀█ █  █ █▀▀█ █▀▀▄ ▄▀▀█",
    "    █    █▀▀█ █  █ █  █ █    █▀▀█",
    "    ▀▀▀▀ ▀  ▀ ▀▀▀▀ ▀▀▀▀ ▀    ▀  ▀",
];

const LABEL_WIDTH: usize = 10;
const LABEL_SESSION: &str = "Session";
const LABEL_MODEL: &str = "Model";
const LABEL_USAGE: &str = "Usage";
const LABEL_SUBSCRIPTION: &str = "On plan";
const LABEL_CONTINUE: &str = "Continue";
const LABEL_SAVED: &str = "Saved";

const SEPARATOR: &str = " · ";
const NOUN_TURN: &str = "turn";
const NOUN_OTHER_SESSION: &str = "other session";
const RESUME_PREFIX: &str = "Resume session: ";
const RESUME_COMMAND: &str = "caudra -s";

const DAY_SECONDS: u64 = 86_400;
const HOUR_SECONDS: u64 = 3_600;
const MINUTE_SECONDS: u64 = 60;
const UNIT_DAY: &str = "d";
const UNIT_HOUR: &str = "h";
const UNIT_MINUTE: &str = "m";
const UNIT_SECOND: &str = "s";

/// The focused session as the exiting run left it: what it was, what it spent,
/// and how to get back into it.
pub struct ExitSummary {
    id: CaudraId,
    title: String,
    model: String,
    usage: TokenUsage,
    cost: Option<f64>,
    subscription_cost: Option<f64>,
    turns: u64,
    run_time: Duration,
    other_sessions: usize,
}

impl ExitSummary {
    /// Reads the session after shutdown checkpointed it, so the usage and the
    /// turn count here are the same totals that landed on disk.
    pub fn new(session: &AppSession, run_time: Duration, other_sessions: usize) -> Self {
        Self {
            id: session.id,
            title: session.title.clone(),
            model: session.model.clone(),
            usage: session.token_usage,
            // Turns settle their own cost, so summing the per-model entries
            // bills the session without pricing anything a second time.
            cost: Self::sum(session.usage_by_model().values().filter_map(|u| u.cost)),
            subscription_cost: Self::sum(
                session
                    .usage_by_model()
                    .values()
                    .filter_map(|u| u.subscription_cost),
            ),
            turns: session.meta.turns,
            run_time,
            other_sessions,
        }
    }

    pub fn banner(&self) -> String {
        let mut out = String::from("\n");
        for row in WORDMARK {
            writeln!(out, "{BOLD}{row}{RESET}").unwrap();
        }
        out.push('\n');
        push_row(&mut out, LABEL_SESSION, &self.session_row());
        push_row(&mut out, LABEL_MODEL, &self.model_row());
        // An unpriced provider still reports tokens; a session that never sent
        // a turn has nothing to report at all.
        if self.usage != TokenUsage::default() {
            push_row(&mut out, LABEL_USAGE, &self.usage.format(self.cost));
        }
        // Its own row rather than a share of the usage line: the figure above is
        // money owed, and a subscription owes none.
        if let Some(subscription) = self.subscription_cost {
            push_row(&mut out, LABEL_SUBSCRIPTION, &format!("${subscription:.4}"));
        }
        push_row(&mut out, LABEL_CONTINUE, &self.resume_command());
        if self.other_sessions > 0 {
            push_row(&mut out, LABEL_SAVED, &self.saved_row());
        }
        out.push('\n');
        out
    }

    fn sum(costs: impl Iterator<Item = f64>) -> Option<f64> {
        costs.reduce(|total, cost| total + cost)
    }

    /// For a redirected stderr, where the block is noise a script has to skip.
    pub fn resume_hint(&self) -> String {
        format!("{RESUME_PREFIX}{}\n", self.resume_command())
    }

    fn resume_command(&self) -> String {
        format!("{RESUME_COMMAND} {}", self.id)
    }

    fn session_row(&self) -> String {
        match self.turns {
            0 => self.title.clone(),
            count => format!("{}{SEPARATOR}{}", self.title, pluralize(count, NOUN_TURN)),
        }
    }

    fn model_row(&self) -> String {
        format!(
            "{}{SEPARATOR}{}",
            self.model,
            format_run_time(self.run_time)
        )
    }

    fn saved_row(&self) -> String {
        pluralize(self.other_sessions as u64, NOUN_OTHER_SESSION)
    }
}

/// The two coarsest units the run reached. A wall clock carries nanosecond
/// precision that nobody exiting a session wants to read.
fn format_run_time(run_time: Duration) -> String {
    let seconds = run_time.as_secs();
    let (days, hours) = (seconds / DAY_SECONDS, seconds % DAY_SECONDS / HOUR_SECONDS);
    let (minutes, rest) = (
        seconds % HOUR_SECONDS / MINUTE_SECONDS,
        seconds % MINUTE_SECONDS,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{rest}{UNIT_SECOND}"),
        (0, 0, _) => two_units(minutes, UNIT_MINUTE, rest, UNIT_SECOND),
        (0, _, _) => two_units(hours, UNIT_HOUR, minutes, UNIT_MINUTE),
        _ => two_units(days, UNIT_DAY, hours, UNIT_HOUR),
    }
}

fn two_units(major: u64, major_unit: &str, minor: u64, minor_unit: &str) -> String {
    match minor {
        0 => format!("{major}{major_unit}"),
        minor => format!("{major}{major_unit} {minor}{minor_unit}"),
    }
}

fn push_row(out: &mut String, label: &str, value: &str) {
    writeln!(out, "  {DIM}{label:<LABEL_WIDTH$}{RESET}{value}").unwrap();
}

fn pluralize(count: u64, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        count => format!("{count} {noun}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TITLE: &str = "Rich session exit summary";
    const MODEL: &str = "anthropic/claude-opus-4-6";
    const COST: f64 = 2.417;
    const RUN_TIME: Duration = Duration::from_secs(750);
    const SPENT: TokenUsage = TokenUsage {
        input: 1_200_000,
        output: 45_300,
        cache_creation: 0,
        cache_read: 0,
    };
    const NO_PRICE: &str = "an unpriced session must not invent a dollar amount";
    const NO_SPEND: &str = "a session that never sent a turn has no usage to report";
    const NO_OTHERS: &str = "a lone tab must not claim other sessions were saved";
    const RESUMABLE: &str = "the banner must always say how to get back in";
    const BRANDED: &str = "the banner must carry the wordmark";
    const ONE_LINE: &str = "a redirected stderr gets exactly one line";

    fn summary(usage: TokenUsage, cost: Option<f64>) -> ExitSummary {
        ExitSummary {
            id: CaudraId::generate(),
            title: TITLE.into(),
            model: MODEL.into(),
            usage,
            cost,
            subscription_cost: None,
            turns: 48,
            run_time: RUN_TIME,
            other_sessions: 0,
        }
    }

    fn spent() -> ExitSummary {
        summary(SPENT, Some(COST))
    }

    #[test]
    fn banner_shows_the_wordmark_and_the_resume_command() {
        let summary = spent();
        let banner = summary.banner();
        assert!(banner.contains(WORDMARK[1]), "{BRANDED}");
        assert!(banner.contains(&summary.resume_command()), "{RESUMABLE}");
    }

    #[test_case(Some(COST) => true  ; "a_priced_session_shows_the_bill")]
    #[test_case(None       => false ; "an_unpriced_session_shows_tokens_only")]
    fn banner_prices_only_what_the_provider_quoted(cost: Option<f64>) -> bool {
        let banner = summary(SPENT, cost).banner();
        assert!(banner.contains(LABEL_USAGE), "{NO_PRICE}");
        banner.contains('$')
    }

    #[test]
    fn banner_drops_the_usage_row_when_nothing_was_spent() {
        let banner = summary(TokenUsage::default(), None).banner();
        assert!(!banner.contains(LABEL_USAGE), "{NO_SPEND}");
        assert!(banner.contains(LABEL_CONTINUE), "{RESUMABLE}");
    }

    #[test_case(1, "1 other session"  ; "one_other_tab_reads_singular")]
    #[test_case(3, "3 other sessions" ; "more_tabs_read_plural")]
    fn saved_row_counts_the_other_tabs(other_sessions: usize, expected: &str) {
        let mut summary = spent();
        summary.other_sessions = other_sessions;
        assert_eq!(summary.saved_row(), expected);
        assert!(summary.banner().contains(expected));
    }

    #[test]
    fn banner_omits_the_saved_row_for_a_single_tab() {
        assert!(!spent().banner().contains(LABEL_SAVED), "{NO_OTHERS}");
    }

    #[test_case(0  => TITLE.to_string()                        ; "an_untouched_session_is_just_its_title")]
    #[test_case(1  => format!("{TITLE}{SEPARATOR}1 turn")      ; "a_single_turn_reads_singular")]
    #[test_case(48 => format!("{TITLE}{SEPARATOR}48 turns")    ; "more_turns_read_plural")]
    fn session_row_counts_the_exchanges(turns: u64) -> String {
        let mut summary = spent();
        summary.turns = turns;
        summary.session_row()
    }

    /// The reported case: an `Instant::elapsed()` carries nanoseconds, and the
    /// summary must not spell every one of them out.
    #[test_case(45_361_273_567_127 => "12h 36m" ; "a_long_run_drops_its_nanoseconds")]
    #[test_case(0                  => "0s"      ; "an_instant_run_still_reads_as_a_duration")]
    #[test_case(45_000_000_000     => "45s"     ; "under_a_minute_is_seconds")]
    #[test_case(150_000_000_000    => "2m 30s"  ; "minutes_keep_their_seconds")]
    #[test_case(5_400_000_000_000  => "1h 30m"  ; "hours_drop_to_minutes")]
    #[test_case(7_200_000_000_000  => "2h"      ; "a_round_hour_is_one_unit")]
    #[test_case(93_600_000_000_000 => "1d 2h"   ; "past_a_day_reads_in_days")]
    fn format_run_time_keeps_the_two_coarsest_units(nanos: u64) -> String {
        format_run_time(Duration::from_nanos(nanos))
    }

    #[test]
    fn resume_hint_stays_on_one_line() {
        let summary = spent();
        let hint = summary.resume_hint();
        assert_eq!(hint.lines().count(), 1, "{ONE_LINE}");
        assert!(hint.contains(&summary.resume_command()), "{RESUMABLE}");
    }
}
