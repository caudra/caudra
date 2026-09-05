//! What the run is worth saying once the TUI is gone. Rendered with plain SGR
//! rather than theme colors: the alternate screen is already down, so the
//! terminal's own background is back and an accent picked for the TUI palette
//! has no contrast guarantee against it.

use std::fmt::Write;
use std::time::Duration;

use caudra_agent::tools::humanize_duration;
use caudra_providers::{HistoryItemKind, TokenUsage, UserOrigin};
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
const LABEL_CONTINUE: &str = "Continue";
const LABEL_SAVED: &str = "Saved";

const SEPARATOR: &str = " · ";
const NOUN_MESSAGE: &str = "message";
const NOUN_OTHER_SESSION: &str = "other session";
const RESUME_PREFIX: &str = "Resume session: ";
const RESUME_COMMAND: &str = "caudra -s";

/// The focused session as the exiting run left it: what it was, what it spent,
/// and how to get back into it.
pub struct ExitSummary {
    id: CaudraId,
    title: String,
    model: String,
    usage: TokenUsage,
    cost: Option<f64>,
    messages: usize,
    run_time: Duration,
    other_sessions: usize,
}

impl ExitSummary {
    /// Reads the session after shutdown checkpointed it, so the usage here is
    /// the same total that landed on disk.
    pub fn new(session: &AppSession, run_time: Duration, other_sessions: usize) -> Self {
        Self {
            id: session.id,
            title: session.title.clone(),
            model: session.model.clone(),
            usage: session.token_usage,
            // Turns settle their own cost, so summing the per-model entries
            // bills the session without pricing anything a second time.
            cost: session
                .usage_by_model()
                .values()
                .filter_map(|usage| usage.cost)
                .reduce(|total, cost| total + cost),
            messages: session
                .messages()
                .iter()
                .filter(|item| is_message(&item.kind))
                .count(),
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
        push_row(&mut out, LABEL_CONTINUE, &self.resume_command());
        if self.other_sessions > 0 {
            push_row(&mut out, LABEL_SAVED, &self.saved_row());
        }
        out.push('\n');
        out
    }

    /// For a redirected stderr, where the block is noise a script has to skip.
    pub fn resume_hint(&self) -> String {
        format!("{RESUME_PREFIX}{}\n", self.resume_command())
    }

    fn resume_command(&self) -> String {
        format!("{RESUME_COMMAND} {}", self.id)
    }

    fn session_row(&self) -> String {
        match self.messages {
            0 => self.title.clone(),
            count => format!(
                "{}{SEPARATOR}{}",
                self.title,
                pluralize(count, NOUN_MESSAGE)
            ),
        }
    }

    fn model_row(&self) -> String {
        format!(
            "{}{SEPARATOR}{}",
            self.model,
            humanize_duration(self.run_time)
        )
    }

    fn saved_row(&self) -> String {
        pluralize(self.other_sessions, NOUN_OTHER_SESSION)
    }
}

fn push_row(out: &mut String, label: &str, value: &str) {
    writeln!(out, "  {DIM}{label:<LABEL_WIDTH$}{RESET}{value}").unwrap();
}

/// Tool calls, results and reasoning are history items too, and counting them
/// would report a number many times what the transcript shows.
fn is_message(kind: &HistoryItemKind) -> bool {
    matches!(
        kind,
        HistoryItemKind::User {
            origin: UserOrigin::Turn,
            ..
        } | HistoryItemKind::AssistantText { .. }
    )
}

fn pluralize(count: usize, noun: &str) -> String {
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
            messages: 48,
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

    #[test_case(0  => TITLE.to_string()                            ; "an_untouched_session_is_just_its_title")]
    #[test_case(1  => format!("{TITLE}{SEPARATOR}1 message")       ; "a_single_message_reads_singular")]
    #[test_case(48 => format!("{TITLE}{SEPARATOR}48 messages")     ; "more_messages_read_plural")]
    fn session_row_counts_the_transcript(messages: usize) -> String {
        let mut summary = spent();
        summary.messages = messages;
        summary.session_row()
    }

    #[test]
    fn resume_hint_stays_on_one_line() {
        let summary = spent();
        let hint = summary.resume_hint();
        assert_eq!(hint.lines().count(), 1, "{ONE_LINE}");
        assert!(hint.contains(&summary.resume_command()), "{RESUMABLE}");
    }
}
