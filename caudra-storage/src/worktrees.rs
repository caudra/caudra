//! Sessions in linked worktrees. Git forgets a worktree once it is removed,
//! leaving nothing on disk that ties its path to the repository, so every
//! linked worktree Caudra sees is recorded while it exists. A session left in
//! one that is gone is then moved back to a checkout that still exists.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::StateDir;
use crate::checkout::{self, CheckoutMember, RepositoryKey};
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, SessionLease, SessionLocation, SessionRelocation,
};
use crate::state::SCOPE_GLOBAL;

const RECORDED_WORKTREES_KEY: &str = "checkouts.linked";

/// Recorded linked worktrees by root.
type Recorded = BTreeMap<String, RecordedWorktree>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecordedWorktree {
    repository: RepositoryKey,
    admin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
}

/// Sessions moved back out of one removed worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedBack {
    /// The worktree's branch, or its directory name when it had none.
    pub worktree: String,
    pub moved: usize,
    /// Left in place because another Caudra has them open or they cannot be
    /// relocated yet. A later pass tries again.
    pub stranded: usize,
}

impl fmt::Display for MovedBack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Moved {} back from removed worktree {}",
            session_count(self.moved),
            self.worktree
        )?;
        if self.stranded > 0 {
            write!(formatter, "; {} more could not move yet", self.stranded)?;
        }
        Ok(())
    }
}

/// A checkout of the repository and the sessions working in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSessions {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// A linked worktree rather than the main checkout.
    pub linked: bool,
    pub sessions: Vec<SessionLocation>,
}

/// Records the linked worktrees of the repository `cwd` is in, then moves each
/// session left in a recorded one that has since been removed back to a
/// checkout that still exists: the one git moved it to, else the same
/// directory in the main checkout, else the main checkout itself. A session
/// another Caudra has open stays until a later pass finds it free.
pub fn reconcile(state_dir: &StateDir, cwd: &Path) -> Result<Vec<MovedBack>, SessionError> {
    if state_dir.is_ephemeral() {
        return Ok(Vec::new());
    }
    match checkout::discover(cwd) {
        Some(checkout) => reconcile_repository(state_dir, &checkout.repository),
        None => Ok(Vec::new()),
    }
}

/// [`reconcile`] for the repository of the removed worktree session `id` works
/// in, so resuming it opens wherever it now lives. Nothing happens while its
/// directory exists.
pub fn reconcile_session(
    state_dir: &StateDir,
    id: CaudraId,
) -> Result<Vec<MovedBack>, SessionError> {
    if state_dir.is_ephemeral() {
        return Ok(Vec::new());
    }
    let database = SessionDatabase::open_state(state_dir)?;
    let Some(cwd) = database.local_session_cwd(id)?.map(PathBuf::from) else {
        return Ok(Vec::new());
    };
    if !is_gone(&cwd) {
        return Ok(Vec::new());
    }
    let recorded: Recorded = database
        .state_get(SCOPE_GLOBAL, RECORDED_WORKTREES_KEY)?
        .unwrap_or_default();
    drop(database);
    match holder(&recorded, &cwd) {
        Some(worktree) => reconcile_repository(state_dir, &worktree.repository),
        None => Ok(Vec::new()),
    }
}

/// The sessions working in each other checkout of the repository `cwd` is in,
/// main checkout first, leaving out checkouts without any.
pub fn sibling_sessions(
    state_dir: &StateDir,
    cwd: &Path,
) -> Result<Vec<CheckoutSessions>, SessionError> {
    let Some(current) = checkout::discover(cwd) else {
        return Ok(Vec::new());
    };
    let members = live_members(&current.repository);
    if members.iter().all(|member| member.root == current.root) {
        return Ok(Vec::new());
    }
    Ok(sessions_by_checkout(state_dir, &members)?
        .into_iter()
        .filter(|listed| listed.root != current.root && !listed.sessions.is_empty())
        .collect())
}

/// Every live checkout of the repository `cwd` is in, main checkout first,
/// with the sessions working in each. Empty outside a repository.
pub fn checkouts(state_dir: &StateDir, cwd: &Path) -> Result<Vec<CheckoutSessions>, SessionError> {
    match checkout::discover(cwd) {
        Some(current) => sessions_by_checkout(state_dir, &live_members(&current.repository)),
        None => Ok(Vec::new()),
    }
}

fn live_members(repository: &RepositoryKey) -> Vec<CheckoutMember> {
    checkout::members(repository)
        .into_iter()
        .filter(|member| !member.prunable)
        .collect()
}

/// A session counts for the innermost checkout holding it, so a worktree
/// nested in the main checkout keeps its own.
fn sessions_by_checkout(
    state_dir: &StateDir,
    members: &[CheckoutMember],
) -> Result<Vec<CheckoutSessions>, SessionError> {
    let database = SessionDatabase::open_state(state_dir)?;
    members
        .iter()
        .map(|member| {
            let sessions = database
                .local_sessions_under(&member.root)?
                .into_iter()
                .filter(|session| innermost(members, Path::new(&session.cwd)) == Some(&member.root))
                .collect();
            Ok(CheckoutSessions {
                root: member.root.clone(),
                branch: member.branch.clone(),
                linked: member.admin.is_some(),
                sessions,
            })
        })
        .collect()
}

fn reconcile_repository(
    state_dir: &StateDir,
    repository: &RepositoryKey,
) -> Result<Vec<MovedBack>, SessionError> {
    let members = checkout::members(repository);
    let mut database = SessionDatabase::open_state(state_dir)?;
    let removed = record(&mut database, repository, &members)?;
    let mut reports = Vec::new();
    let mut settled = Vec::new();
    for (root, worktree) in removed {
        let report = move_back(
            &mut database,
            state_dir,
            Path::new(&root),
            &worktree,
            &members,
        )?;
        if report.stranded == 0 {
            settled.push(root);
        }
        if report.moved > 0 {
            tracing::info!(
                worktree = %report.worktree,
                moved = report.moved,
                stranded = report.stranded,
                "moved sessions back out of a removed worktree"
            );
            reports.push(report);
        }
    }
    if !settled.is_empty() {
        database.state_update(
            SCOPE_GLOBAL,
            RECORDED_WORKTREES_KEY,
            |recorded: &mut Recorded| {
                for root in &settled {
                    recorded.remove(root);
                }
            },
        )?;
    }
    Ok(reports)
}

/// Records each live linked worktree in `members`, forgets the worktrees of
/// repositories that are gone, and returns this repository's recorded
/// worktrees whose root is gone. Writes only when the record changed.
fn record(
    database: &mut SessionDatabase,
    repository: &RepositoryKey,
    members: &[CheckoutMember],
) -> Result<Vec<(String, RecordedWorktree)>, SessionError> {
    let outcome = database.state_try_update(
        SCOPE_GLOBAL,
        RECORDED_WORKTREES_KEY,
        |recorded: &mut Recorded| {
            let before = recorded.clone();
            recorded.retain(|_, worktree| !is_gone(worktree.repository.as_path()));
            for member in members.iter().filter(|member| !member.prunable) {
                if let Some(admin) = &member.admin {
                    recorded.insert(
                        member.root.to_string_lossy().into_owned(),
                        RecordedWorktree {
                            repository: repository.clone(),
                            admin: admin.clone(),
                            branch: member.branch.clone(),
                        },
                    );
                }
            }
            let removed = recorded
                .iter()
                .filter(|(root, worktree)| {
                    worktree.repository == *repository && is_gone(Path::new(root))
                })
                .map(|(root, worktree)| (root.clone(), worktree.clone()))
                .collect();
            // Returned as the error when nothing changed, which leaves the row
            // unwritten.
            if *recorded == before {
                Err(removed)
            } else {
                Ok(removed)
            }
        },
    )?;
    Ok(outcome.unwrap_or_else(|removed| removed))
}

/// Moves the sessions under `root`, a removed worktree, to the first of its
/// [`targets`] that exists, along with the usage recorded for each directory
/// every session left.
fn move_back(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    root: &Path,
    worktree: &RecordedWorktree,
    members: &[CheckoutMember],
) -> Result<MovedBack, SessionError> {
    let targets = targets(root, worktree, members);
    let mut by_cwd: BTreeMap<String, Vec<SessionLocation>> = BTreeMap::new();
    for session in database.local_sessions_under(root)? {
        by_cwd.entry(session.cwd.clone()).or_default().push(session);
    }
    let mut report = MovedBack {
        worktree: worktree.branch.clone().unwrap_or_else(|| label(root)),
        moved: 0,
        stranded: 0,
    };
    for (cwd, sessions) in by_cwd {
        let Some(destination) = destination(root, Path::new(&cwd), &targets) else {
            report.stranded += sessions.len();
            continue;
        };
        let count = sessions.len();
        let moved = sessions
            .iter()
            .filter(|session| relocate(database, state_dir, session, &destination))
            .count();
        report.moved += moved;
        report.stranded += count - moved;
        if moved == count {
            database.relocate_project_usage(&cwd, &destination.to_string_lossy())?;
        }
    }
    Ok(report)
}

/// Whether `session` moved to `destination`. One open in another Caudra, or
/// one that cannot be relocated yet, is left for a later pass.
fn relocate(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    session: &SessionLocation,
    destination: &Path,
) -> bool {
    let result = SessionLease::acquire(state_dir, session.id).and_then(|_lease| {
        database.relocate_sessions(&SessionRelocation {
            sessions: vec![session.clone()],
            source_cwd: None,
            destination: destination.to_string_lossy().into_owned(),
            include_project_usage: false,
            keep_plan: true,
        })
    });
    match result {
        Ok(result) => result.sessions_moved == 1,
        Err(SessionError::SessionInUse { .. }) => false,
        Err(error) => {
            tracing::warn!(
                %error,
                session_id = %session.id,
                cwd = %session.cwd,
                "could not move a session back out of a removed worktree"
            );
            false
        }
    }
}

/// Where a removed worktree's sessions go, in order: the checkout git moved it
/// to, when it was moved rather than removed, then every live checkout, the
/// main one first.
fn targets(root: &Path, worktree: &RecordedWorktree, members: &[CheckoutMember]) -> Vec<PathBuf> {
    let live = members
        .iter()
        .filter(|member| !member.prunable && member.root != root);
    let moved = live.clone().filter(|member| {
        member.admin.as_deref() == Some(worktree.admin.as_str()) && member.branch == worktree.branch
    });
    moved
        .chain(live)
        .map(|member| member.root.clone())
        .collect()
}

/// The first target that exists, entered at the directory `cwd` was in below
/// `root` when the target has it too.
fn destination(root: &Path, cwd: &Path, targets: &[PathBuf]) -> Option<PathBuf> {
    let relative = cwd.strip_prefix(root).ok()?;
    targets.iter().find_map(|target| {
        let target = target.canonicalize().ok()?;
        let nested = target
            .join(relative)
            .canonicalize()
            .ok()
            .filter(|nested| nested.is_dir() && nested.starts_with(&target));
        Some(nested.unwrap_or(target))
    })
}

/// The recorded worktree holding `cwd`, the innermost when one is nested in
/// another.
fn holder<'a>(recorded: &'a Recorded, cwd: &Path) -> Option<&'a RecordedWorktree> {
    recorded
        .iter()
        .filter(|(root, _)| cwd.starts_with(root))
        .max_by_key(|(root, _)| root.len())
        .map(|(_, worktree)| worktree)
}

fn innermost<'a>(members: &'a [CheckoutMember], cwd: &Path) -> Option<&'a PathBuf> {
    members
        .iter()
        .map(|member| &member.root)
        .filter(|root| cwd.starts_with(root))
        .max_by_key(|root| root.components().count())
}

/// Gone for certain, rather than unreadable for a moment.
fn is_gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn label(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn session_count(count: usize) -> String {
    match count {
        1 => "1 session".to_owned(),
        count => format!("{count} sessions"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::checkout::fixture::{
        ADMIN, BRANCH, Links, canonical_tempdir, linked_pair, linked_pair_with_state,
        linked_worktree,
    };
    use crate::projects::GIT_MARKER;
    use crate::sessions::{Session, TitleSource};

    const MODEL: &str = "test-model";
    const NESTED: &str = "crates/app";
    const MOVED_DIR: &str = "moved";
    const WORKTREES_DIR: &str = "worktrees";
    const NOT_MOVED: &str = "a session must stay where it is until its worktree is gone";
    const NOT_BACK: &str = "a session left in a removed worktree must move back";

    #[derive(Clone, Serialize, Deserialize)]
    struct Message;

    impl TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    type TestSession = Session<Message, Value, Value>;

    fn session_in(state_dir: &StateDir, cwd: &Path) -> CaudraId {
        let mut session = TestSession::new(MODEL, &cwd.to_string_lossy());
        session.save(state_dir).unwrap();
        session.id
    }

    fn cwd_of(state_dir: &StateDir, id: CaudraId) -> PathBuf {
        SessionDatabase::open_state(state_dir)
            .unwrap()
            .local_session_cwd(id)
            .unwrap()
            .map(PathBuf::from)
            .unwrap()
    }

    /// What `git worktree remove` leaves: neither the checkout nor its admin
    /// directory.
    fn remove_worktree(main: &Path, worktree: &Path) {
        fs::remove_dir_all(worktree).unwrap();
        fs::remove_dir_all(main.join(GIT_MARKER).join(WORKTREES_DIR).join(ADMIN)).unwrap();
    }

    #[test]
    fn a_session_in_a_removed_worktree_moves_back_to_the_main_checkout() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let id = session_in(&state_dir, &worktree);
        assert!(reconcile(&state_dir, &main).unwrap().is_empty());
        assert_eq!(cwd_of(&state_dir, id), worktree, "{NOT_MOVED}");
        remove_worktree(&main, &worktree);

        let moved = reconcile(&state_dir, &main).unwrap();

        assert_eq!(
            moved,
            vec![MovedBack {
                worktree: BRANCH.into(),
                moved: 1,
                stranded: 0,
            }]
        );
        assert_eq!(cwd_of(&state_dir, id), main, "{NOT_BACK}");
        assert!(reconcile(&state_dir, &main).unwrap().is_empty());
    }

    #[test_case(true ; "into_the_same_directory_when_main_has_it")]
    #[test_case(false ; "into_the_main_root_when_it_does_not")]
    fn a_nested_session_moves_back(main_has_it: bool) {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        fs::create_dir_all(worktree.join(NESTED)).unwrap();
        if main_has_it {
            fs::create_dir_all(main.join(NESTED)).unwrap();
        }
        let id = session_in(&state_dir, &worktree.join(NESTED));
        reconcile(&state_dir, &worktree).unwrap();
        remove_worktree(&main, &worktree);

        reconcile(&state_dir, &main).unwrap();

        let expected = if main_has_it { main.join(NESTED) } else { main };
        assert_eq!(cwd_of(&state_dir, id), expected, "{NOT_BACK}");
    }

    #[test]
    fn a_worktree_git_moved_takes_its_sessions_along() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let id = session_in(&state_dir, &worktree);
        reconcile(&state_dir, &main).unwrap();
        let moved = worktree.with_file_name(MOVED_DIR);
        linked_worktree(
            &main.join(GIT_MARKER),
            ADMIN,
            &moved,
            BRANCH,
            Links::Absolute,
        );
        fs::remove_dir_all(&worktree).unwrap();

        reconcile(&state_dir, &main).unwrap();

        assert_eq!(cwd_of(&state_dir, id), moved);
    }

    #[test]
    fn a_session_open_elsewhere_moves_once_it_is_free() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let id = session_in(&state_dir, &worktree);
        reconcile(&state_dir, &main).unwrap();
        remove_worktree(&main, &worktree);
        let lease = SessionLease::acquire(&state_dir, id).unwrap();

        assert!(reconcile(&state_dir, &main).unwrap().is_empty());
        assert_eq!(cwd_of(&state_dir, id), worktree, "{NOT_MOVED}");

        drop(lease);
        assert_eq!(reconcile(&state_dir, &main).unwrap().len(), 1);
        assert_eq!(cwd_of(&state_dir, id), main, "{NOT_BACK}");
    }

    #[test]
    fn a_directory_never_recorded_as_a_worktree_is_left_alone() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let id = session_in(&state_dir, &worktree);
        remove_worktree(&main, &worktree);

        assert!(reconcile(&state_dir, &main).unwrap().is_empty());
        assert!(reconcile_session(&state_dir, id).unwrap().is_empty());
        assert_eq!(cwd_of(&state_dir, id), worktree);
    }

    #[test]
    fn resuming_a_session_in_a_removed_worktree_moves_it_back_first() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let id = session_in(&state_dir, &worktree);
        reconcile(&state_dir, &worktree).unwrap();
        remove_worktree(&main, &worktree);

        assert_eq!(reconcile_session(&state_dir, id).unwrap().len(), 1);
        assert_eq!(cwd_of(&state_dir, id), main, "{NOT_BACK}");
    }

    #[test]
    fn other_checkouts_list_their_sessions_under_the_innermost_checkout() {
        let (_temp, base) = canonical_tempdir();
        let state_dir = StateDir::from_path(base.join("state"));
        let (main, outer) = linked_pair(&base);
        let nested = main.join(NESTED);
        linked_worktree(
            &main.join(GIT_MARKER),
            MOVED_DIR,
            &nested,
            BRANCH,
            Links::Absolute,
        );
        let in_main = session_in(&state_dir, &main);
        let in_nested = session_in(&state_dir, &nested);
        session_in(&state_dir, &outer);

        let listed: Vec<_> = sibling_sessions(&state_dir, &outer)
            .unwrap()
            .into_iter()
            .map(|checkout| {
                (
                    checkout.root,
                    checkout
                        .sessions
                        .into_iter()
                        .map(|session| session.id)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        assert_eq!(
            listed,
            vec![(main, vec![in_main]), (nested, vec![in_nested])]
        );
    }

    #[test]
    fn every_checkout_is_listed_with_its_sessions_main_first() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let in_worktree = session_in(&state_dir, &worktree);

        let listed: Vec<_> = checkouts(&state_dir, &worktree)
            .unwrap()
            .into_iter()
            .map(|checkout| {
                (
                    checkout.root,
                    checkout.linked,
                    checkout
                        .sessions
                        .into_iter()
                        .map(|session| session.id)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        assert_eq!(
            listed,
            vec![(main, false, vec![]), (worktree, true, vec![in_worktree])]
        );
    }

    #[test_case(1, 0, "Moved 1 session back from removed worktree feature/login" ; "one")]
    #[test_case(2, 1, "Moved 2 sessions back from removed worktree feature/login; 1 more could not move yet" ; "some_stranded")]
    fn the_report_says_what_moved(moved: usize, stranded: usize, expected: &str) {
        let report = MovedBack {
            worktree: BRANCH.into(),
            moved,
            stranded,
        };

        assert_eq!(report.to_string(), expected);
    }
}
