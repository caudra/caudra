//! Carries out a `/worktree` change between two UI generations, once every
//! session is saved and its agent has stopped. Part of it moves this process
//! and part of it hands sessions to another Herdr pane, neither of which a
//! running UI can do.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use color_eyre::Result;
use color_eyre::eyre::eyre;

use caudra_agent::herdr::{HerdrEnv, OpenedWorkspace, resume_command_line};
use caudra_agent::worktree::{
    self, Backend, CreateRequest, Removal, RemoveRequest, Request, counterpart, label,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{SessionDatabase, SessionRelocation};
use caudra_ui::{SessionRelocationHandoff, SessionTab};

const CREATE_FAILED: &str = "Could not create the worktree";
const CARRY_FAILED: &str = "Uncommitted changes did not carry over";
const MOVE_FAILED: &str = "The session stayed where it was";
const REMOVE_FAILED: &str = "Could not remove worktree";
const REMOVAL_KEPT: &str =
    "The worktree was kept, because the sessions working in it could not move out";
const PANE_FAILED: &str = "Herdr could not start the session in its new pane";
const NOT_SAVED: &str = "is not saved";

pub(super) struct Executed {
    pub(super) handled: Handled,
    /// What the user is told once the UI is back.
    pub(super) notes: Vec<String>,
}

pub(super) enum Handled {
    /// The UI comes back on these tabs, where it was.
    Continue {
        tabs: Vec<SessionTab>,
        focused: usize,
    },
    /// The tabs go through the relocation path, and `follow_up` finishes the
    /// change once they have.
    Relocate {
        tabs: Vec<SessionTab>,
        focused: usize,
        handoff: SessionRelocationHandoff,
        follow_up: Box<FollowUp>,
    },
    /// The sessions went on in another Herdr pane, and this one closes.
    Exit,
}

/// What is left of a worktree change once its sessions have moved.
pub(super) struct FollowUp {
    moved: String,
    removal: Option<(Removal, String)>,
}

impl FollowUp {
    /// What is said once the relocation settled, given what it said itself.
    /// Only once the sessions moved is the checkout they left removed, and
    /// the change then speaks for the relocation.
    pub(super) fn finish(
        self,
        relocation: Vec<String>,
        committed: bool,
        backend: &Backend,
        storage: &StateDir,
    ) -> Vec<String> {
        if !committed {
            let mut notes = relocation;
            notes.extend(self.removal.map(|_| REMOVAL_KEPT.to_owned()));
            return notes;
        }
        let mut notes = vec![self.moved];
        if let Some((removal, name)) = self.removal {
            notes.extend(remove_checkout(&removal, &name, backend, storage));
        }
        notes
    }
}

impl Executed {
    fn resumed(tabs: Vec<SessionTab>, focused: usize, notes: Vec<String>) -> Self {
        Self {
            handled: Handled::Continue { tabs, focused },
            notes,
        }
    }
}

pub(super) fn execute(
    request: Request,
    tabs: Vec<SessionTab>,
    focused: usize,
    storage: &StateDir,
    backend: &Backend,
) -> Executed {
    match request {
        Request::Create(request) => create(&request, tabs, focused, storage, backend),
        Request::Remove(request) => remove(&request, tabs, focused, storage, backend),
    }
}

fn create(
    request: &CreateRequest,
    tabs: Vec<SessionTab>,
    focused: usize,
    storage: &StateDir,
    backend: &Backend,
) -> Executed {
    let created = match worktree::create(backend, request) {
        Ok(created) => created,
        Err(error) => {
            return Executed::resumed(tabs, focused, vec![format!("{CREATE_FAILED}: {error}")]);
        }
    };
    let name = label(created.branch.as_deref(), &created.root);
    let mut notes = vec![format!(
        "Created worktree {name} at {}",
        created.root.display()
    )];
    notes.extend(
        created
            .carry_failed
            .map(|reason| format!("{CARRY_FAILED}: {reason}")),
    );
    let destination = counterpart(&request.cwd, &request.source, &created.root);
    let relocation = match relocation(storage, &[request.session], &destination) {
        Ok(relocation) => relocation,
        Err(error) => {
            notes.push(format!("{MOVE_FAILED}: {error:#}"));
            return Executed::resumed(tabs, focused, notes);
        }
    };
    let moved = format!("This session moved into worktree {name}");
    match (created.workspace, backend.herdr()) {
        (Some(workspace), Some(herdr)) => {
            let hand_off = HandOff {
                id: request.session,
                relocation: &relocation,
                workspace: &workspace,
                name: &name,
                herdr,
            };
            hand_off.run(tabs, focused, storage, notes, moved)
        }
        _ => Executed {
            handled: Handled::Relocate {
                tabs,
                focused,
                handoff: handoff(relocation),
                follow_up: Box::new(FollowUp {
                    moved,
                    removal: None,
                }),
            },
            notes,
        },
    }
}

/// A session moving into a new worktree that Herdr opened a pane for.
struct HandOff<'a> {
    id: CaudraId,
    relocation: &'a SessionRelocation,
    workspace: &'a OpenedWorkspace,
    /// What the new tab and pane are labelled, the worktree's branch.
    name: &'a str,
    herdr: &'a HerdrEnv,
}

impl HandOff<'_> {
    /// Moves the session while its tab still holds it, lets go of it, and has
    /// Herdr resume it in the new pane. A process left without tabs exits once
    /// the session runs there; otherwise it stays to say how to resume it.
    fn run(
        self,
        mut tabs: Vec<SessionTab>,
        focused: usize,
        storage: &StateDir,
        mut notes: Vec<String>,
        moved: String,
    ) -> Executed {
        if let Err(error) = relocate(storage, self.relocation) {
            notes.push(format!("{MOVE_FAILED}: {error:#}"));
            return Executed::resumed(tabs, focused, notes);
        }
        let mut focused = focused;
        if let Some(index) = tabs.iter().position(|tab| tab.session.id == self.id) {
            drop(tabs.remove(index));
            if index < focused {
                focused -= 1;
            }
        }
        let cli = self.herdr.cli();
        let OpenedWorkspace {
            tab_id, pane_id, ..
        } = self.workspace;
        if let Err(error) = cli
            .tab_rename(tab_id, self.name)
            .and_then(|()| cli.pane_rename(pane_id, self.name))
        {
            tracing::warn!(%error, tab = tab_id, pane = pane_id, "Herdr did not label the new tab and pane");
        }
        let command = resume_command_line(&self.id.to_string());
        match cli.pane_run(pane_id, &command) {
            Ok(()) => {
                notes.push(format!("{moved} and went on in its Herdr workspace"));
                if tabs.is_empty() {
                    return Executed {
                        handled: Handled::Exit,
                        notes,
                    };
                }
            }
            Err(error) => notes.push(format!(
                "{PANE_FAILED}: {error}. Resume it there with: {command}"
            )),
        }
        let focused = focused.min(tabs.len().saturating_sub(1));
        Executed::resumed(tabs, focused, notes)
    }
}

fn remove(
    request: &RemoveRequest,
    tabs: Vec<SessionTab>,
    focused: usize,
    storage: &StateDir,
    backend: &Backend,
) -> Executed {
    let name = label(request.branch.as_deref(), &request.root);
    let mut notes = super::reconcile_worktrees(storage, &request.root);
    let (removal, stash) = match Removal::prepare(backend, request) {
        Ok(prepared) => prepared,
        Err(error) => {
            notes.push(format!("{REMOVE_FAILED} {name}: {error}"));
            return Executed::resumed(tabs, focused, notes);
        }
    };
    notes.extend(stash.map(|stash| {
        format!(
            "Stashed the uncommitted changes of {name}; `git stash apply {stash}` brings them back in any checkout"
        )
    }));
    let working: Vec<_> = tabs
        .iter()
        .filter_map(|tab| {
            let cwd = canonical(&tab.session.cwd);
            cwd.starts_with(&removal.root)
                .then_some((tab.session.id, cwd))
        })
        .collect();
    let destination = working.first().map_or_else(
        || {
            tabs.get(focused).map_or_else(
                || removal.main_root.clone(),
                |tab| canonical(&tab.session.cwd),
            )
        },
        |(_, cwd)| counterpart(cwd, &removal.root, &removal.main_root),
    );
    let ids: Vec<_> = working.iter().map(|(id, _)| *id).collect();
    if let (Some(herdr), Some(workspace)) = (backend.herdr(), removal.workspace.as_deref())
        && herdr.workspace_id.as_deref() == Some(workspace)
    {
        let leaving = Leaving {
            herdr,
            removal: &removal,
            workspace,
            name: &name,
            destination: &destination,
        };
        return leaving.run(tabs, focused, &ids, storage, notes);
    }
    if ids.is_empty() {
        notes.extend(remove_checkout(&removal, &name, backend, storage));
        return Executed::resumed(tabs, focused, notes);
    }
    let relocation = match relocation(storage, &ids, &destination) {
        Ok(relocation) => relocation,
        Err(error) => {
            notes.push(format!("{REMOVE_FAILED} {name}: {error:#}"));
            return Executed::resumed(tabs, focused, notes);
        }
    };
    let moved = format!(
        "Moved {} back to {}",
        session_count(ids.len()),
        destination.display()
    );
    Executed {
        handled: Handled::Relocate {
            tabs,
            focused,
            handoff: handoff(relocation),
            follow_up: Box::new(FollowUp {
                moved,
                removal: Some((removal, name)),
            }),
        },
        notes,
    }
}

/// Leaving the Herdr workspace of a checkout being removed, which Herdr
/// closes along with this pane.
struct Leaving<'a> {
    herdr: &'a HerdrEnv,
    removal: &'a Removal,
    /// The workspace Herdr closes, which holds this pane.
    workspace: &'a str,
    name: &'a str,
    destination: &'a Path,
}

impl Leaving<'_> {
    /// Opens a pane in the repository's own workspace, or in a new one, moves
    /// the sessions working in the checkout, resumes the focused session in
    /// that pane, and leaves the removal to run once this process is gone.
    fn run(
        self,
        tabs: Vec<SessionTab>,
        focused: usize,
        working: &[CaudraId],
        storage: &StateDir,
        mut notes: Vec<String>,
    ) -> Executed {
        let cli = self.herdr.cli();
        let pane = match &self.removal.source_workspace {
            Some(workspace) => cli.split_workspace(workspace, self.destination),
            None => cli
                .workspace_create(self.destination)
                .map(|opened| opened.pane_id),
        };
        let pane = match pane {
            Ok(pane) => pane,
            Err(error) => {
                notes.push(format!("{REMOVE_FAILED} {}: {error}", self.name));
                return Executed::resumed(tabs, focused, notes);
            }
        };
        if !working.is_empty()
            && let Err(error) = relocation(storage, working, self.destination)
                .and_then(|relocation| relocate(storage, &relocation))
        {
            notes.push(format!("{REMOVE_FAILED} {}: {error:#}", self.name));
            return Executed::resumed(tabs, focused, notes);
        }
        let resumed = tabs.get(focused).map(|tab| tab.session.id);
        drop(tabs);
        if let Some(id) = resumed {
            let command = resume_command_line(&id.to_string());
            if let Err(error) = cli.pane_run(&pane, &command) {
                notes.push(format!(
                    "{PANE_FAILED}: {error}. Resume it there with: {command}"
                ));
                notes.push(REMOVAL_KEPT.into());
                return Executed {
                    handled: Handled::Exit,
                    notes,
                };
            }
        }
        notes.push(self.start_removal());
        Executed {
            handled: Handled::Exit,
            notes,
        }
    }

    /// Starts the removal in a process group of its own, which outlives this
    /// pane when Herdr closes it.
    fn start_removal(&self) -> String {
        let mut command = self
            .herdr
            .cli()
            .worktree_remove_command(self.workspace, self.removal.force);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        match command.spawn() {
            Ok(child) => {
                drop(child);
                format!("Removing worktree {}; its branch is kept", self.name)
            }
            Err(error) => format!("{REMOVE_FAILED} {}: {error}", self.name),
        }
    }
}

/// Removes the checkout, then moves back the sessions still recorded in it.
fn remove_checkout(
    removal: &Removal,
    name: &str,
    backend: &Backend,
    storage: &StateDir,
) -> Vec<String> {
    let mut notes = vec![match removal.run(backend) {
        Ok(()) => format!("Removed worktree {name}; its branch is kept"),
        Err(error) => format!("{REMOVE_FAILED} {name}: {error}"),
    }];
    notes.extend(super::reconcile_worktrees(storage, &removal.main_root));
    notes
}

/// Moves the saved sessions `ids` to `destination`, keeping their plans,
/// since every checkout of a repository shares its project state.
fn relocation(
    storage: &StateDir,
    ids: &[CaudraId],
    destination: &Path,
) -> Result<SessionRelocation> {
    let inventory = SessionDatabase::open_state(storage)?.local_session_locations()?;
    let sessions = ids
        .iter()
        .map(|id| {
            inventory
                .iter()
                .find(|location| location.id == *id)
                .cloned()
                .ok_or_else(|| eyre!("session {id} {NOT_SAVED}"))
        })
        .collect::<Result<_>>()?;
    Ok(SessionRelocation {
        sessions,
        source_cwd: None,
        destination: destination.to_string_lossy().into_owned(),
        include_project_usage: false,
        keep_plan: true,
    })
}

fn relocate(storage: &StateDir, relocation: &SessionRelocation) -> Result<()> {
    SessionDatabase::open_state(storage)?.relocate_sessions(relocation)?;
    Ok(())
}

fn handoff(request: SessionRelocation) -> SessionRelocationHandoff {
    SessionRelocationHandoff {
        request,
        donor: None,
        leases: Vec::new(),
    }
}

fn canonical(cwd: &str) -> PathBuf {
    Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd))
}

fn session_count(count: usize) -> String {
    match count {
        1 => "1 session".into(),
        count => format!("{count} sessions"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    use caudra_agent::herdr::{HerdrEnv, OpenedWorkspace, resume_command_line};
    use caudra_agent::worktree::{
        Backend, Changes, CreateRequest, RemoveRequest, Request, checkout_path,
    };
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{SessionDatabase, SessionLease};
    use caudra_ui::{AppSession, SessionTab};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CREATE_FAILED, Executed, HandOff, Handled, PANE_FAILED, REMOVAL_KEPT, execute, relocate,
        relocation,
    };

    const MODEL: &str = "test/model";
    const BRANCH: &str = "feature/login";
    const HEAD: &str = "HEAD";
    const MISSING_BASE: &str = "missing-revision";
    const LINKED: &str = "linked";
    const WORKTREES: &str = "worktrees";
    const WORKSPACE: &str = "w2";
    const TAB: &str = "w2:t1";
    const PANE: &str = "w2:p1";
    const LABEL: &str = "login";
    const RECORD: &str = "argv";
    const HERDR_SUCCEEDS: &str = "exit 0";
    const HERDR_FAILS: &str = "echo 'no such pane' >&2; exit 1";
    const MOVED: &str = "This session moved";
    const RELOCATION_NOTE: &str = "Session relocation was not committed";

    struct Repository {
        _temp: TempDir,
        base: PathBuf,
        root: PathBuf,
        storage: StateDir,
        backend: Backend,
    }

    impl Repository {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let root = base.join("main");
            fs::create_dir(&root).unwrap();
            for args in [
                &["init", "--quiet", "--initial-branch=main"][..],
                &["config", "user.email", "caudra@example.com"],
                &["config", "user.name", "Caudra"],
                &["config", "commit.gpgsign", "false"],
                &["commit", "--quiet", "--allow-empty", "--message", "initial"],
            ] {
                git(&root, args);
            }
            Self {
                storage: StateDir::from_path(base.join("state")),
                backend: Backend::Git {
                    directory: Some(base.join(WORKTREES)),
                },
                _temp: temp,
                base,
                root,
            }
        }

        fn tab(&self, cwd: &Path) -> SessionTab {
            let mut session = AppSession::new(MODEL, &cwd.to_string_lossy());
            session.save(&self.storage).unwrap();
            let lease = Arc::new(SessionLease::acquire(&self.storage, session.id).unwrap());
            SessionTab {
                session,
                lease,
                cursor: None,
            }
        }

        fn worktree(&self) -> PathBuf {
            let path = self.base.join(LINKED);
            git(
                &self.root,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    BRANCH,
                    &path.to_string_lossy(),
                ],
            );
            path
        }

        fn create(&self, tabs: Vec<SessionTab>, base: &str) -> Executed {
            let request = CreateRequest {
                session: tabs[0].session.id,
                cwd: self.root.clone(),
                source: self.root.clone(),
                main_root: self.root.clone(),
                branch: Some(BRANCH.into()),
                base: base.into(),
                carry: false,
            };
            execute(
                Request::Create(request),
                tabs,
                0,
                &self.storage,
                &self.backend,
            )
        }

        fn remove(&self, tabs: Vec<SessionTab>, root: &Path) -> Executed {
            let request = RemoveRequest {
                root: root.to_path_buf(),
                main_root: self.root.clone(),
                branch: Some(BRANCH.into()),
                changes: Changes::None,
            };
            execute(
                Request::Remove(request),
                tabs,
                0,
                &self.storage,
                &self.backend,
            )
        }

        fn stored_cwd(&self, id: CaudraId) -> String {
            SessionDatabase::open_state(&self.storage)
                .unwrap()
                .local_session_cwd(id)
                .unwrap()
                .unwrap()
        }

        /// A Herdr that records every call it gets, one argument per line,
        /// then runs `outcome`.
        fn herdr(&self, outcome: &str) -> HerdrEnv {
            let binary = self.base.join("herdr");
            let record = self.base.join(RECORD);
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{outcome}\n",
                    record.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            HerdrEnv {
                binary: binary.into(),
                socket_path: self.base.join("herdr.sock").into(),
                pane_id: PANE.into(),
                workspace_id: None,
            }
        }

        fn herdr_calls(&self) -> Vec<String> {
            fs::read_to_string(self.base.join(RECORD))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// Hands session `moved` to the root pane of a new worktree's
        /// workspace, through a Herdr that runs `outcome` for every call.
        fn hand_off(
            &self,
            tabs: Vec<SessionTab>,
            focused: usize,
            moved: CaudraId,
            outcome: &str,
        ) -> (Executed, PathBuf) {
            let worktree = self.worktree();
            let herdr = self.herdr(outcome);
            let request = relocation(&self.storage, &[moved], &worktree).unwrap();
            let workspace = OpenedWorkspace {
                workspace_id: WORKSPACE.into(),
                tab_id: TAB.into(),
                pane_id: PANE.into(),
                already_open: false,
            };
            let hand_off = HandOff {
                id: moved,
                relocation: &request,
                workspace: &workspace,
                name: LABEL,
                herdr: &herdr,
            };
            let executed = hand_off.run(tabs, focused, &self.storage, Vec::new(), MOVED.into());
            (executed, worktree)
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn ids(tabs: &[SessionTab]) -> Vec<CaudraId> {
        tabs.iter().map(|tab| tab.session.id).collect()
    }

    #[test]
    fn a_worktree_that_cannot_be_created_leaves_the_session_where_it_was() {
        let repository = Repository::new();
        let tab = repository.tab(&repository.root);
        let id = tab.session.id;

        let Executed { handled, notes } = repository.create(vec![tab], MISSING_BASE);

        let Handled::Continue { tabs, focused } = handled else {
            panic!("a failed create must not move the session");
        };
        assert_eq!(ids(&tabs), [id]);
        assert_eq!(focused, 0);
        assert!(notes[0].starts_with(CREATE_FAILED), "{notes:?}");
        assert_eq!(repository.stored_cwd(id), repository.root.to_string_lossy());
    }

    #[test]
    fn a_git_worktree_takes_the_session_along_with_its_plan() {
        let repository = Repository::new();
        let tab = repository.tab(&repository.root);
        let id = tab.session.id;

        let Executed { handled, .. } = repository.create(vec![tab], HEAD);

        let Handled::Relocate { handoff, .. } = handled else {
            panic!("a git worktree moves this process along");
        };
        let created = checkout_path(&repository.base.join(WORKTREES), &repository.root, BRANCH);
        assert!(created.is_dir());
        assert_eq!(handoff.request.destination, created.to_string_lossy());
        assert_eq!(
            handoff
                .request
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            [id]
        );
        assert!(handoff.request.keep_plan);
    }

    #[test]
    fn a_worktree_no_session_works_in_is_removed_at_once() {
        let repository = Repository::new();
        let worktree = repository.worktree();
        let tab = repository.tab(&repository.root);
        let id = tab.session.id;

        let Executed { handled, .. } = repository.remove(vec![tab], &worktree);

        let Handled::Continue { tabs, .. } = handled else {
            panic!("nothing has to move out first");
        };
        assert_eq!(ids(&tabs), [id]);
        assert!(!worktree.exists());
    }

    #[test_case(true ; "committed")]
    #[test_case(false ; "not_committed")]
    fn a_worktree_goes_only_once_its_sessions_moved_out(committed: bool) {
        let repository = Repository::new();
        let worktree = repository.worktree();
        let tab = repository.tab(&worktree);
        let id = tab.session.id;

        let Executed { handled, .. } = repository.remove(vec![tab], &worktree);
        let Handled::Relocate {
            handoff, follow_up, ..
        } = handled
        else {
            panic!("the session working in the worktree moves out first");
        };
        assert_eq!(
            handoff.request.destination,
            repository.root.to_string_lossy()
        );
        if committed {
            relocate(&repository.storage, &handoff.request).unwrap();
        }
        let notes = follow_up.finish(
            vec![RELOCATION_NOTE.into()],
            committed,
            &repository.backend,
            &repository.storage,
        );

        assert_eq!(worktree.exists(), !committed);
        assert_eq!(notes.iter().any(|note| note == REMOVAL_KEPT), !committed);
        assert_eq!(notes.iter().any(|note| note == RELOCATION_NOTE), !committed);
        let expected = if committed {
            &repository.root
        } else {
            &worktree
        };
        assert_eq!(repository.stored_cwd(id), expected.to_string_lossy());
    }

    #[test_case(0, 2, 2 ; "earlier_session_moves")]
    #[test_case(1, 0, 0 ; "later_session_moves")]
    #[test_case(2, 2, 1 ; "focused_session_moves")]
    fn a_session_handed_to_a_herdr_pane_is_let_go_of_here(
        moving: usize,
        focused: usize,
        refocused: usize,
    ) {
        let repository = Repository::new();
        let tabs: Vec<_> = (0..3).map(|_| repository.tab(&repository.root)).collect();
        let original = ids(&tabs);
        let moved = original[moving];

        let (Executed { handled, .. }, worktree) =
            repository.hand_off(tabs, focused, moved, HERDR_SUCCEEDS);

        let Handled::Continue { tabs, focused } = handled else {
            panic!("the tabs left here carry on");
        };
        let staying: Vec<_> = original.iter().copied().filter(|id| *id != moved).collect();
        assert_eq!(ids(&tabs), staying);
        assert_eq!(tabs[focused].session.id, original[refocused]);
        assert_eq!(repository.stored_cwd(moved), worktree.to_string_lossy());
        assert!(SessionLease::acquire(&repository.storage, moved).is_ok());
        let resume = resume_command_line(&moved.to_string());
        assert_eq!(
            repository.herdr_calls(),
            [
                "tab", "rename", TAB, LABEL, "pane", "rename", PANE, LABEL, "pane", "run", PANE,
                &resume
            ]
        );
    }

    /// The pane is left to its shell only once the session runs elsewhere;
    /// until then this process stays, and says how to resume it.
    #[test_case(HERDR_SUCCEEDS, true ; "session_started")]
    #[test_case(HERDR_FAILS, false ; "session_not_started")]
    fn handing_off_the_last_session_exits_once_it_runs_there(outcome: &str, exits: bool) {
        let repository = Repository::new();
        let tab = repository.tab(&repository.root);
        let moved = tab.session.id;

        let (Executed { handled, notes }, worktree) =
            repository.hand_off(vec![tab], 0, moved, outcome);

        match handled {
            Handled::Exit => assert!(exits, "{notes:?}"),
            Handled::Continue { tabs, .. } => {
                assert!(!exits && tabs.is_empty(), "{notes:?}");
                assert!(notes.iter().any(|note| note.starts_with(PANE_FAILED)));
            }
            Handled::Relocate { .. } => panic!("Herdr hand-offs never relocate this process"),
        }
        assert_eq!(repository.stored_cwd(moved), worktree.to_string_lossy());
        assert!(repository.herdr_calls().iter().any(|call| call == "run"));
    }
}
