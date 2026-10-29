//! What a project's memory nudges have done so far: which sessions ran, when
//! each nudge went out, and which sessions wrote a note before any nudge
//! asked. The agent reads it to space nudges across sessions and to stop
//! once the model keeps memory on its own.

use rusqlite::{OptionalExtension, params};

use crate::memory_journal::{MemoryJournal, MemoryJournalError};

const SESSION_EVENT: &str = "session";
const NUDGE_EVENT: &str = "nudge";
const UNPROMPTED_EVENT: &str = "unprompted";
const GRADUATED_EVENT: &str = "graduated";
const RECORD_OPERATION: &str = "memory nudge record";
/// How many of a project's newest sessions decide whether it graduated.
pub const GRADUATION_WINDOW: u32 = 3;
/// How many of those sessions must have written a note unprompted.
pub const GRADUATION_SESSIONS: u32 = 2;

/// Idempotent, because tests that replay older migrations keep the table.
pub(crate) const TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS memory_nudges (
    scope      TEXT NOT NULL,
    session_id TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK(kind IN ('session', 'nudge', 'unprompted', 'graduated')),
    created_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS memory_nudges_scope ON memory_nudges(scope, kind, created_ms);
"#;

/// Graduation holds once [`GRADUATION_SESSIONS`] of the project's newest
/// [`GRADUATION_WINDOW`] sessions wrote a note unprompted.
const GRADUATION: &str = "SELECT count(*) >= ?3 FROM (
    SELECT session_id FROM memory_nudges WHERE scope = ?1 AND kind = 'session'
    GROUP BY session_id ORDER BY max(created_ms) DESC LIMIT ?2
) newest WHERE EXISTS (
    SELECT 1 FROM memory_nudges written WHERE written.scope = ?1
    AND written.kind = 'unprompted' AND written.session_id = newest.session_id
)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeEvent {
    /// A main session that could be nudged started.
    Session,
    Nudge,
    /// The model wrote a note no nudge had asked for.
    Unprompted,
}

impl NudgeEvent {
    const fn storage_name(self) -> &'static str {
        match self {
            Self::Session => SESSION_EVENT,
            Self::Nudge => NUDGE_EVENT,
            Self::Unprompted => UNPROMPTED_EVENT,
        }
    }
}

/// Where a project stands with nudges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NudgeStanding {
    /// Nudges sent in every session of the project since the asked time.
    pub nudges_since: u32,
    /// The model keeps memory on its own, so the project is never nudged
    /// again.
    pub graduated: bool,
}

impl MemoryJournal {
    pub fn record_nudge_event(
        &self,
        scope: &str,
        session_id: &str,
        event: NudgeEvent,
        now_ms: i64,
    ) -> Result<(), MemoryJournalError> {
        self.write_one(
            RECORD_OPERATION,
            "INSERT INTO memory_nudges (scope, session_id, kind, created_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![scope, session_id, event.storage_name(), now_ms],
        )?;
        Ok(())
    }

    /// The project's standing, counting nudges from `since_ms` on. The
    /// first time graduation holds it is recorded, so it lasts whatever
    /// later sessions do.
    pub fn nudge_standing(
        &self,
        scope: &str,
        session_id: &str,
        since_ms: i64,
        now_ms: i64,
    ) -> Result<NudgeStanding, MemoryJournalError> {
        let connection = self.connection();
        let nudges_since = connection.query_row(
            "SELECT count(*) FROM memory_nudges WHERE scope = ?1 AND kind = ?2 \
             AND created_ms >= ?3",
            params![scope, NUDGE_EVENT, since_ms],
            |row| row.get(0),
        )?;
        let recorded = connection
            .query_row(
                "SELECT 1 FROM memory_nudges WHERE scope = ?1 AND kind = ?2 LIMIT 1",
                params![scope, GRADUATED_EVENT],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let graduated = recorded
            || connection.query_row(
                GRADUATION,
                params![scope, GRADUATION_WINDOW, GRADUATION_SESSIONS],
                |row| row.get(0),
            )?;
        if graduated && !recorded {
            self.write_one(
                RECORD_OPERATION,
                "INSERT INTO memory_nudges (scope, session_id, kind, created_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![scope, session_id, GRADUATED_EVENT, now_ms],
            )?;
        }
        Ok(NudgeStanding {
            nudges_since,
            graduated,
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::{NudgeEvent, NudgeStanding};
    use crate::StateDir;
    use crate::memory_journal::MemoryJournal;

    const SCOPE: &str = "project-a";
    const OTHER_SCOPE: &str = "project-b";
    const CURRENT: &str = "current";
    const DAY_START_MS: i64 = 1_000;
    const NOW_MS: i64 = 5_000;

    fn fixture() -> (TempDir, MemoryJournal) {
        let root = tempdir().unwrap();
        let journal = MemoryJournal::open(&StateDir::from_path(root.path().to_path_buf())).unwrap();
        (root, journal)
    }

    fn standing(journal: &MemoryJournal) -> NudgeStanding {
        journal
            .nudge_standing(SCOPE, CURRENT, DAY_START_MS, NOW_MS)
            .unwrap()
    }

    /// Records one session per flag, oldest first, each writing unprompted
    /// when its flag is set.
    fn sessions(journal: &MemoryJournal, wrote: &[bool]) {
        for (at, &wrote) in (0..).zip(wrote) {
            let session = format!("session-{at}");
            journal
                .record_nudge_event(SCOPE, &session, NudgeEvent::Session, at)
                .unwrap();
            if wrote {
                journal
                    .record_nudge_event(SCOPE, &session, NudgeEvent::Unprompted, at)
                    .unwrap();
            }
        }
    }

    #[test_case(&[], false ; "no_sessions")]
    #[test_case(&[true], false ; "one_writing_session")]
    #[test_case(&[true, false, true], true ; "two_of_the_last_three")]
    #[test_case(&[true, true, false, false, false], false ; "writes_older_than_the_window")]
    #[test_case(&[false, false, true, true], true ; "two_newest_sessions")]
    fn graduation_needs_two_of_the_newest_three_sessions(wrote: &[bool], graduated: bool) {
        let (_root, journal) = fixture();
        sessions(&journal, wrote);

        assert_eq!(standing(&journal).graduated, graduated);
    }

    #[test]
    fn graduation_lasts_whatever_later_sessions_do() {
        let (_root, journal) = fixture();
        sessions(&journal, &[true, true]);
        assert!(standing(&journal).graduated);

        for later in 0..3 {
            journal
                .record_nudge_event(
                    SCOPE,
                    &format!("later-{later}"),
                    NudgeEvent::Session,
                    NOW_MS,
                )
                .unwrap();
        }

        assert!(standing(&journal).graduated);
    }

    #[test]
    fn nudges_count_from_the_asked_time_within_the_scope() {
        let (_root, journal) = fixture();
        for (scope, at) in [
            (SCOPE, DAY_START_MS - 1),
            (SCOPE, DAY_START_MS),
            (SCOPE, NOW_MS),
            (OTHER_SCOPE, NOW_MS),
        ] {
            journal
                .record_nudge_event(scope, CURRENT, NudgeEvent::Nudge, at)
                .unwrap();
        }

        assert_eq!(standing(&journal).nudges_since, 2);
    }

    #[test]
    fn purge_forgets_the_scopes_nudges() {
        let (_root, journal) = fixture();
        sessions(&journal, &[true, true]);
        journal
            .record_nudge_event(SCOPE, CURRENT, NudgeEvent::Nudge, NOW_MS)
            .unwrap();

        journal.purge(SCOPE).unwrap();

        assert_eq!(standing(&journal), NudgeStanding::default());
    }
}
