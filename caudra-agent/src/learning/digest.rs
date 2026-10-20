//! What a learner reads first: the exchanges a person took part in since the
//! last run, oldest first, redacted, each tagged with the reference
//! `history.read` takes, and cut to a byte budget.
//!
//! An exchange is a typed user message, the answer it replied to, and how that
//! answer went: cut off, a call refused, a tool failing. Those are where a
//! person corrects the agent, which is most of what is worth learning.

use std::collections::HashSet;
use std::fmt::Write;

use caudra_storage::history_search::{HistoryKind, TimelineItem};

use crate::history::iso_time;
use crate::permissions::review::redact_text;

pub const DIGEST_BYTES: usize = 48 * 1024;
pub const USER_BYTES: usize = 4 * 1024;
pub const ASSISTANT_BYTES: usize = 1024;
/// The kinds a digest is built from.
pub const KINDS: [HistoryKind; 3] = [
    HistoryKind::User,
    HistoryKind::Assistant,
    HistoryKind::ToolResult,
];
const CUT_MARK: &str = " […]";
const INTERRUPTED: &str = "(the user cut this answer off)";
const REFUSED: &str = "calls refused";
const FAILED: &str = "tool calls failed";

/// The digest of a window of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub text: String,
    /// Exchanges in the text.
    pub exchanges: usize,
    /// Sessions the exchanges came from.
    pub sessions: usize,
    /// Where the next run starts: the end of the window, or the first
    /// exchange that did not fit.
    pub through_ms: u64,
    /// More history was left for the next run.
    pub truncated: bool,
}

#[derive(Default)]
struct Pending<'a> {
    answer: Option<&'a TimelineItem>,
    refused: usize,
    failed: usize,
    start_ms: Option<u64>,
}

impl<'a> Pending<'a> {
    fn note(&mut self, item: &'a TimelineItem) {
        self.start_ms.get_or_insert(item.at_ms);
        match item.kind {
            HistoryKind::Assistant => self.answer = Some(item),
            HistoryKind::ToolResult => {
                self.refused += usize::from(item.refused);
                self.failed += usize::from(item.failed && !item.refused);
            }
            _ => {}
        }
    }

    fn render(&self, user: &TimelineItem) -> String {
        let mut text = format!(
            "### {} · {} · {}\n",
            user.reference,
            iso_time(user.at_ms),
            one_line(&user.session_title)
        );
        if let Some(answer) = self.answer {
            let mark = match answer.interrupted {
                true => format!(" {INTERRUPTED}"),
                false => String::new(),
            };
            let _ = writeln!(
                text,
                "assistant{mark}: {}",
                excerpt(&answer.text, ASSISTANT_BYTES)
            );
        }
        if self.refused > 0 {
            let _ = writeln!(text, "({} {REFUSED})", self.refused);
        }
        if self.failed > 0 {
            let _ = writeln!(text, "({} {FAILED})", self.failed);
        }
        let _ = writeln!(text, "user: {}\n", excerpt(&user.text, USER_BYTES));
        text
    }
}

impl Digest {
    /// The digest of `items`, which cover history up to `until_ms`.
    pub fn build(items: &[TimelineItem], until_ms: u64) -> Self {
        let mut digest = Self {
            text: String::new(),
            exchanges: 0,
            sessions: 0,
            through_ms: until_ms,
            truncated: false,
        };
        let mut sessions = HashSet::new();
        let mut pending = Pending::default();
        for item in items {
            if item.kind != HistoryKind::User {
                pending.note(item);
                continue;
            }
            if !item.authored {
                pending = Pending::default();
                continue;
            }
            let exchange = pending.render(item);
            if digest.text.len() + exchange.len() > DIGEST_BYTES {
                digest.through_ms = pending.start_ms.unwrap_or(item.at_ms);
                digest.truncated = true;
                break;
            }
            digest.text.push_str(&exchange);
            digest.exchanges += 1;
            sessions.insert(item.reference.session);
            pending = Pending::default();
        }
        digest.sessions = sessions.len();
        digest
    }
}

/// Typed user messages in `items`, which is what a run waits for.
pub fn authored_messages(items: &[TimelineItem]) -> usize {
    items.iter().filter(|item| item.authored).count()
}

/// At most `limit` bytes of `text`, redacted, cut on whitespace.
fn excerpt(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let cut = trimmed.floor_char_boundary(limit);
    if cut >= trimmed.len() {
        return redact_text(trimmed);
    }
    let head = &trimmed[..cut];
    let head = head
        .rsplit_once(char::is_whitespace)
        .map_or(head, |(head, _)| head);
    format!("{}{CUT_MARK}", redact_text(head))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use caudra_storage::history_search::{HistoryRef, HistoryStream};
    use caudra_storage::id::CaudraId;
    use test_case::test_case;

    use super::*;

    const TITLE: &str = "fix the flaky test";
    const ASK: &str = "use nextest, never cargo test";
    const ANSWER: &str = "Running cargo test now";
    const AUTOMATION: &str = "automation says hello";
    const SECRET: &str = "export OPENAI_API_KEY=sk-live-0123456789abcdefghijklmnop";
    const SECRET_VALUE: &str = "sk-live-0123456789abcdefghijklmnop";
    const UNTIL_MS: u64 = 1_000_000;

    fn item(kind: HistoryKind, at_ms: u64, text: &str) -> TimelineItem {
        TimelineItem {
            reference: HistoryRef {
                session: CaudraId::generate(),
                stream: HistoryStream::Main,
                item: CaudraId::generate(),
            },
            session_title: TITLE.to_owned(),
            at_ms,
            kind,
            text: text.to_owned(),
            authored: kind == HistoryKind::User,
            interrupted: false,
            refused: false,
            failed: false,
        }
    }

    #[test]
    fn an_exchange_carries_the_answer_and_how_it_went() {
        let mut answer = item(HistoryKind::Assistant, 1, ANSWER);
        answer.interrupted = true;
        let mut refused = item(HistoryKind::ToolResult, 2, "");
        refused.refused = true;
        let user = item(HistoryKind::User, 3, ASK);

        let digest = Digest::build(&[answer, refused, user.clone()], UNTIL_MS);

        assert!(digest.text.contains(&user.reference.to_string()));
        assert!(digest.text.contains(INTERRUPTED));
        assert!(digest.text.contains(REFUSED));
        assert!(digest.text.find(ANSWER) < digest.text.find(ASK));
        assert_eq!((digest.exchanges, digest.through_ms), (1, UNTIL_MS));
        assert!(!digest.truncated);
    }

    #[test]
    fn messages_no_person_typed_are_left_out() {
        let mut automation = item(HistoryKind::User, 1, AUTOMATION);
        automation.authored = false;
        let digest = Digest::build(&[automation.clone()], UNTIL_MS);
        assert_eq!(digest.exchanges, 0);
        assert_eq!(authored_messages(&[automation]), 0);
    }

    #[test]
    fn secrets_are_redacted() {
        let digest = Digest::build(&[item(HistoryKind::User, 1, SECRET)], UNTIL_MS);
        assert!(!digest.text.contains(SECRET_VALUE), "{}", digest.text);
    }

    #[test_case(USER_BYTES ; "a_message_at_its_cap")]
    #[test_case(USER_BYTES * 4 ; "a_message_over_its_cap")]
    fn a_long_message_is_cut_to_its_cap(length: usize) {
        let long = "word ".repeat(length / 5);
        let digest = Digest::build(&[item(HistoryKind::User, 1, &long)], UNTIL_MS);
        assert!(digest.text.len() < USER_BYTES + ASSISTANT_BYTES);
    }

    #[test]
    fn the_budget_stops_before_the_exchange_that_overflows_it() {
        let long = "word ".repeat(USER_BYTES / 5);
        let items: Vec<TimelineItem> = (0..DIGEST_BYTES / USER_BYTES + 2)
            .map(|at| item(HistoryKind::User, at as u64 + 1, &long))
            .collect();

        let digest = Digest::build(&items, UNTIL_MS);

        assert!(digest.truncated);
        assert!(digest.text.len() <= DIGEST_BYTES);
        assert_eq!(digest.through_ms, items[digest.exchanges].at_ms);
    }
}
