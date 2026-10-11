//! Which release the user last ran, so Caudra can show what is new once after an update. A global state row.

use std::cmp::Ordering;
use std::mem;

use semver::Version;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::sessions::SessionDatabase;
use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir, StorageError};

const CHANGELOG: StateKey = StateKey {
    name: "ui.changelog",
    class: StateClass::Persistent,
};

/// The last release that ran, and the notes owed for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogMark {
    pub seen: String,
    /// The release before the update that made `seen` current, which the notes start after. `None` shows only
    /// `seen`'s notes.
    pub since: Option<String>,
    /// The notes for the update have not been shown yet.
    pub pending: bool,
}

/// What a start leaves the stored mark as. `Unchanged` keeps the row as it is, without a write.
struct Unchanged;

/// Records that `current` started, and claims the notes owed for its update when `open_now` says the screen is
/// free for them. Returns `Some(since)` to the one start that claims them: a second process starting the same
/// release, or this one later, gets `None`. A start that cannot show them leaves them owed to the next.
///
/// A first start with no mark owes the notes of `current` only when sessions were saved before it, which is an
/// existing user meeting this feature, never a new install. Nothing is owed after a rollback.
pub fn on_start(
    dir: &StateDir,
    current: &str,
    open_now: bool,
) -> Result<Option<Option<String>>, StorageError> {
    if dir.is_ephemeral() {
        return Ok(None);
    }
    let mut database = SessionDatabase::open_state(&dir.for_class(CHANGELOG.class))?;
    let has_sessions = database.has_sessions()?;
    let claimed = database.state_try_update(SCOPE_GLOBAL, CHANGELOG.name, |mark| {
        advance(mark, current, has_sessions, open_now)
    })?;
    Ok(claimed.unwrap_or(None))
}

/// The mark, or `None` when there is none or it cannot be read.
pub fn read(dir: &StateDir) -> Option<ChangelogMark> {
    state::get(dir, SCOPE_GLOBAL, CHANGELOG).unwrap_or_else(|error| {
        warn!(%error, "failed to read the changelog mark");
        None
    })
}

/// Settles the notes owed, because the user opened them without waiting for the start that would have.
pub fn acknowledge(dir: &StateDir) {
    if dir.is_ephemeral() {
        return;
    }
    let settled = state::try_update(
        dir,
        SCOPE_GLOBAL,
        CHANGELOG,
        |mark: &mut Option<ChangelogMark>| match mark {
            Some(mark) if mark.pending => {
                mark.pending = false;
                Ok(())
            }
            _ => Err(Unchanged),
        },
    );
    if let Err(error) = settled {
        warn!(%error, "failed to settle the changelog mark");
    }
}

fn advance(
    mark: &mut Option<ChangelogMark>,
    current: &str,
    has_sessions: bool,
    open_now: bool,
) -> Result<Option<Option<String>>, Unchanged> {
    let version = Version::parse(current).map_err(|_| Unchanged)?;
    let fresh = |pending| ChangelogMark {
        seen: current.to_owned(),
        since: None,
        pending,
    };
    let mut changed = true;
    match mark {
        None => *mark = Some(fresh(has_sessions)),
        Some(existing) => match Version::parse(&existing.seen) {
            Ok(seen) if version.cmp_precedence(&seen) == Ordering::Greater => {
                let previous = mem::replace(&mut existing.seen, current.to_owned());
                // Notes still owed from an update never shown reach back to where those started.
                existing.since = match existing.pending {
                    true => existing.since.take().or(Some(previous)),
                    false => Some(previous),
                };
                existing.pending = true;
            }
            Ok(_) => changed = false,
            Err(_) => *existing = fresh(false),
        },
    }
    let claimable = mark
        .as_mut()
        .filter(|mark| open_now && mark.pending && mark.seen == current);
    match (claimable, changed) {
        (Some(mark), _) => {
            mark.pending = false;
            Ok(Some(mark.since.clone()))
        }
        (None, true) => Ok(None),
        (None, false) => Err(Unchanged),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::sessions::Session;

    const PREVIEW_2: &str = "0.2.0-preview.2";
    const PREVIEW_3: &str = "0.2.0-preview.3";
    const PREVIEW_4: &str = "0.2.0-preview.4";
    const MODEL: &str = "test/model";
    const CWD: &str = "/project";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn mark(seen: &str, since: Option<&str>, pending: bool) -> ChangelogMark {
        ChangelogMark {
            seen: seen.to_owned(),
            since: since.map(str::to_owned),
            pending,
        }
    }

    fn seed(dir: &StateDir, mark: &ChangelogMark) {
        state::set(dir, SCOPE_GLOBAL, CHANGELOG, mark).unwrap();
    }

    #[test]
    fn a_new_install_owes_nothing() {
        let (_temp, dir) = state_dir();
        assert_eq!(on_start(&dir, PREVIEW_4, true).unwrap(), None);
        assert_eq!(read(&dir), Some(mark(PREVIEW_4, None, false)));
    }

    #[test]
    fn an_existing_user_without_a_mark_sees_the_current_notes() {
        let (_temp, dir) = state_dir();
        let mut session: Session<Value, Value, Value> = Session::new(MODEL, CWD);
        session.save(&dir).unwrap();
        assert_eq!(on_start(&dir, PREVIEW_4, true).unwrap(), Some(None));
        assert_eq!(read(&dir), Some(mark(PREVIEW_4, None, false)));
    }

    #[test_case(mark(PREVIEW_2, None, false), PREVIEW_4 => (Some(Some(PREVIEW_2.to_owned())), mark(PREVIEW_4, Some(PREVIEW_2), false)) ; "update_claims")]
    #[test_case(mark(PREVIEW_4, None, false), PREVIEW_4 => (None, mark(PREVIEW_4, None, false)) ; "same_version")]
    #[test_case(mark(PREVIEW_4, Some(PREVIEW_3), true), PREVIEW_3 => (None, mark(PREVIEW_4, Some(PREVIEW_3), true)) ; "rollback")]
    #[test_case(mark("garbage", None, true), PREVIEW_4 => (None, mark(PREVIEW_4, None, false)) ; "unreadable_seen_resets")]
    #[test_case(mark(PREVIEW_3, Some(PREVIEW_2), true), PREVIEW_4 => (Some(Some(PREVIEW_2.to_owned())), mark(PREVIEW_4, Some(PREVIEW_2), false)) ; "unshown_notes_carry_over")]
    fn a_start_advances_the_mark(
        before: ChangelogMark,
        current: &str,
    ) -> (Option<Option<String>>, ChangelogMark) {
        let (_temp, dir) = state_dir();
        seed(&dir, &before);
        let claimed = on_start(&dir, current, true).unwrap();
        (claimed, read(&dir).unwrap())
    }

    #[test]
    fn a_busy_start_defers_the_notes_to_the_next() {
        let (_temp, dir) = state_dir();
        seed(&dir, &mark(PREVIEW_3, None, false));
        assert_eq!(on_start(&dir, PREVIEW_4, false).unwrap(), None);
        assert_eq!(read(&dir), Some(mark(PREVIEW_4, Some(PREVIEW_3), true)));
        assert_eq!(
            on_start(&dir, PREVIEW_4, true).unwrap(),
            Some(Some(PREVIEW_3.to_owned()))
        );
    }

    #[test]
    fn only_the_first_start_claims_the_notes() {
        let (_temp, dir) = state_dir();
        seed(&dir, &mark(PREVIEW_3, None, false));
        assert!(on_start(&dir, PREVIEW_4, true).unwrap().is_some());
        assert_eq!(on_start(&dir, PREVIEW_4, true).unwrap(), None);
    }

    #[test]
    fn acknowledging_settles_owed_notes() {
        let (_temp, dir) = state_dir();
        seed(&dir, &mark(PREVIEW_4, Some(PREVIEW_3), true));
        acknowledge(&dir);
        assert_eq!(read(&dir), Some(mark(PREVIEW_4, Some(PREVIEW_3), false)));
        assert_eq!(on_start(&dir, PREVIEW_4, true).unwrap(), None);
    }

    #[test]
    fn an_ephemeral_run_neither_reads_nor_writes() {
        let temp = TempDir::new().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let dir = StateDir::split(
            temp.path().join("volatile"),
            persistent.path().to_path_buf(),
        );
        seed(&persistent, &mark(PREVIEW_3, None, false));
        assert_eq!(on_start(&dir, PREVIEW_4, true).unwrap(), None);
        acknowledge(&dir);
        assert_eq!(read(&persistent), Some(mark(PREVIEW_3, None, false)));
    }
}
