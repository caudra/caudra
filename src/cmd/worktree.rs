//! Carries out a `/worktree` change between two UI generations, once every
//! session is saved and its agent has stopped. Part of it moves this process
//! and part of it hands sessions to another pane of the terminal host,
//! neither of which a
//! running UI can do.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use color_eyre::Result;
use color_eyre::eyre::eyre;

use caudra_agent::worktree::{
    self, Backend, Changes, CloseRequest, CreateRequest, Removal, RemoveRequest, Request,
    counterpart, label,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{SessionDatabase, SessionRelocation};
use caudra_terminal_host::HostError;
use caudra_terminal_host::resume::resume_command_line;
use caudra_terminal_host::workspaces::{HostLocation, HostPane, Opened, PaneSite, Workspaces};
use caudra_ui::{SessionRelocationHandoff, SessionTab};

const CREATE_FAILED: &str = "Could not create the worktree";
const CARRY_FAILED: &str = "Uncommitted changes did not carry over";
const MOVE_FAILED: &str = "The session stayed where it was";
const REMOVE_FAILED: &str = "Could not remove worktree";
const CLOSE_FAILED: &str = "Could not close worktree";
const CHANGES_KEPT: &str = "Uncommitted changes stayed in worktree";
const REMOVAL_KEPT: &str =
    "The worktree was kept, because the sessions working in it could not move out";
const PANE_FAILED: &str = "could not start the session in its new pane";
const HOST_DECLINED: &str = "Git made the worktree instead";
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
    /// The sessions went on in another pane of the host, and this one closes.
    Exit,
}

/// What is left of a worktree change once its sessions have moved.
pub(super) struct FollowUp {
    moved: String,
    retiring: Option<Retiring>,
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
            notes.extend(self.retiring.map(|_| REMOVAL_KEPT.to_owned()));
            return notes;
        }
        let mut notes = vec![self.moved];
        if let Some(retiring) = self.retiring {
            notes.extend(retiring.finish(backend, storage));
        }
        notes
    }
}

/// A checkout that goes once its sessions moved out.
struct Retiring {
    removal: Removal,
    name: String,
    /// Its uncommitted changes come back to the main checkout first, and it
    /// stays when they cannot.
    carry_back: bool,
}

impl Retiring {
    fn finish(&self, backend: &Backend, storage: &StateDir) -> Vec<String> {
        let mut notes = match self.bring_back() {
            Ok(notes) => notes,
            Err(note) => return vec![note],
        };
        notes.extend(remove_checkout(&self.removal, &self.name, backend, storage));
        notes
    }

    /// Carries the uncommitted changes back when asked to; `Err` says why
    /// they and the checkout stay.
    fn bring_back(&self) -> Result<Vec<String>, String> {
        if !self.carry_back {
            return Ok(Vec::new());
        }
        let name = &self.name;
        match worktree::carry_back(&self.removal.root, &self.removal.main_root) {
            Ok(stashed) => Ok([format!("Brought the uncommitted changes of {name} back")]
                .into_iter()
                .chain(stashed)
                .collect()),
            Err(reason) => Err(format!("{CHANGES_KEPT} {name}, which was kept: {reason}")),
        }
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
        Request::Close(request) => close(&request, tabs, focused, storage, backend),
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
            .host_declined
            .map(|reason| format!("{HOST_DECLINED}: {reason}")),
    );
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
    match (created.opened, backend.host()) {
        (Some(opened), Some(host)) => {
            let hand_off = HandOff {
                id: request.session,
                relocation: &relocation,
                opened: &opened,
                name: &name,
                host,
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
                    retiring: None,
                }),
            },
            notes,
        },
    }
}

/// A session moving into a new worktree that the host opened a pane for.
struct HandOff<'a> {
    id: CaudraId,
    relocation: &'a SessionRelocation,
    opened: &'a Opened,
    /// What the new pane is labelled, the worktree's branch.
    name: &'a str,
    host: &'a dyn Workspaces,
}

impl HandOff<'_> {
    /// Moves the session while its tab still holds it, lets go of it, and has
    /// the host resume it in the new pane. A process left without tabs exits once
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
        let host = self.host.host();
        let command = resume_command_line(&self.id.to_string());
        match self.host.run(&self.opened.pane, &command, Some(self.name)) {
            Ok(()) => {
                notes.push(format!("{moved} and went on in its {host} workspace"));
                if tabs.is_empty() {
                    return Executed {
                        handled: Handled::Exit,
                        notes,
                    };
                }
            }
            Err(error) => notes.push(format!(
                "{host} {PANE_FAILED}: {error}. Resume it there with: {command}"
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
    let retiring = Retiring {
        removal,
        name,
        carry_back: false,
    };
    retiring.leave(tabs, focused, storage, backend, notes)
}

/// Fast-forwards the main checkout to the worktree's commit, then has the
/// worktree go as a removal does, its uncommitted changes coming back once
/// its sessions moved.
fn close(
    request: &CloseRequest,
    tabs: Vec<SessionTab>,
    focused: usize,
    storage: &StateDir,
    backend: &Backend,
) -> Executed {
    let name = label(request.branch.as_deref(), &request.root);
    let mut notes = super::reconcile_worktrees(storage, &request.root);
    let prepared = worktree::plan_close(&request.main_root, &request.root).and_then(|plan| {
        worktree::fast_forward_main(&request.main_root, &plan)?;
        let (removal, _) = Removal::prepare(
            backend,
            &RemoveRequest {
                root: request.root.clone(),
                main_root: request.main_root.clone(),
                branch: request.branch.clone(),
                changes: Changes::None,
            },
        )?;
        Ok((plan, removal))
    });
    let (plan, removal) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            notes.push(format!("{CLOSE_FAILED} {name}: {error}"));
            return Executed::resumed(tabs, focused, notes);
        }
    };
    if plan.commits > 0 {
        notes.push(format!(
            "Fast-forwarded the main checkout by {} from {name}",
            count(plan.commits, "commit")
        ));
    }
    let retiring = Retiring {
        removal,
        name,
        carry_back: plan.carries,
    };
    retiring.leave(tabs, focused, storage, backend, notes)
}

impl Retiring {
    /// Moves the sessions working in the checkout back to the main checkout,
    /// then has it go.
    fn leave(
        self,
        tabs: Vec<SessionTab>,
        focused: usize,
        storage: &StateDir,
        backend: &Backend,
        mut notes: Vec<String>,
    ) -> Executed {
        let root = &self.removal.root;
        let working: Vec<_> = tabs
            .iter()
            .filter_map(|tab| {
                let cwd = canonical(&tab.session.cwd);
                cwd.starts_with(root).then_some((tab.session.id, cwd))
            })
            .collect();
        let destination = working.first().map_or_else(
            || {
                tabs.get(focused).map_or_else(
                    || self.removal.main_root.clone(),
                    |tab| canonical(&tab.session.cwd),
                )
            },
            |(_, cwd)| counterpart(cwd, root, &self.removal.main_root),
        );
        let ids: Vec<_> = working.iter().map(|(id, _)| *id).collect();
        if let (Some(host), Some(location)) = (backend.host(), self.removal.location.as_ref())
            && host.holds_this_pane(location)
        {
            let leaving = Leaving {
                host,
                retiring: &self,
                location,
                destination: &destination,
            };
            return leaving.run(tabs, focused, &ids, storage, notes);
        }
        if ids.is_empty() {
            notes.extend(self.finish(backend, storage));
            return Executed::resumed(tabs, focused, notes);
        }
        let relocation = match relocation(storage, &ids, &destination) {
            Ok(relocation) => relocation,
            Err(error) => {
                notes.push(format!("{REMOVE_FAILED} {}: {error:#}", self.name));
                return Executed::resumed(tabs, focused, notes);
            }
        };
        let moved = format!(
            "Moved {} back to {}",
            count(ids.len(), "session"),
            destination.display()
        );
        Executed {
            handled: Handled::Relocate {
                tabs,
                focused,
                handoff: handoff(relocation),
                follow_up: Box::new(FollowUp {
                    moved,
                    retiring: Some(self),
                }),
            },
            notes,
        }
    }
}

/// Leaving the host's workspace of a checkout that goes, which the host
/// closes along with this pane.
struct Leaving<'a> {
    host: &'a dyn Workspaces,
    retiring: &'a Retiring,
    /// Where the host has the checkout open, which holds this pane.
    location: &'a HostLocation,
    /// Where the sessions working in the checkout move to.
    destination: &'a Path,
}

/// A session to resume in a tab of its own.
struct Resuming {
    id: CaudraId,
    title: String,
    cwd: PathBuf,
}

impl Leaving<'_> {
    /// Opens a tab in the repository's own workspace, or in a new one, moves
    /// the sessions working in the checkout, brings its changes back when
    /// asked to, and resumes every session here in a tab of its own, the
    /// focused one last so it keeps the focus. The removal runs once this
    /// process is gone, and only when every session runs elsewhere.
    fn run(
        self,
        tabs: Vec<SessionTab>,
        focused: usize,
        working: &[CaudraId],
        storage: &StateDir,
        mut notes: Vec<String>,
    ) -> Executed {
        let name = &self.retiring.name;
        let resuming = self.resuming(&tabs, focused, working);
        let first = match resuming.first() {
            Some(session) => {
                let site = match &self.retiring.removal.main_location {
                    Some(location) => PaneSite::TabIn {
                        location,
                        cwd: &session.cwd,
                    },
                    None => PaneSite::NewWorkspace(&session.cwd),
                };
                match self.host.pane(site) {
                    Ok(pane) => Some(pane),
                    Err(error) => {
                        notes.push(format!("{REMOVE_FAILED} {name}: {error}"));
                        return Executed::resumed(tabs, focused, notes);
                    }
                }
            }
            None => None,
        };
        if !working.is_empty()
            && let Err(error) = relocation(storage, working, self.destination)
                .and_then(|relocation| relocate(storage, &relocation))
        {
            notes.push(format!("{REMOVE_FAILED} {name}: {error:#}"));
            return Executed::resumed(tabs, focused, notes);
        }
        drop(tabs);
        let mut removing = match self.retiring.bring_back() {
            Ok(brought) => {
                notes.extend(brought);
                true
            }
            Err(note) => {
                notes.push(note);
                false
            }
        };
        let mut started = true;
        if let Some(first) = &first {
            for (index, session) in resuming.iter().enumerate() {
                let pane = match index {
                    0 => Ok(first.clone()),
                    _ => self.host.pane(PaneSite::TabBeside {
                        pane: first,
                        cwd: &session.cwd,
                    }),
                };
                if let Err(note) = self.resume(pane, session) {
                    notes.push(note);
                    started = false;
                }
            }
        }
        if removing && !started {
            notes.push(REMOVAL_KEPT.into());
            removing = false;
        }
        if removing {
            notes.push(self.start_removal());
        }
        Executed {
            handled: Handled::Exit,
            notes,
        }
    }

    /// The sessions of `tabs` in the order their tabs open, the focused one
    /// last, each where it works once the move is done.
    fn resuming(&self, tabs: &[SessionTab], focused: usize, working: &[CaudraId]) -> Vec<Resuming> {
        let mut ordered: Vec<_> = tabs.iter().collect();
        if focused < ordered.len() {
            let tab = ordered.remove(focused);
            ordered.push(tab);
        }
        ordered
            .into_iter()
            .map(|tab| Resuming {
                id: tab.session.id,
                title: tab.session.title.clone(),
                cwd: if working.contains(&tab.session.id) {
                    self.destination.to_path_buf()
                } else {
                    canonical(&tab.session.cwd)
                },
            })
            .collect()
    }

    /// Resumes `session` in `pane`, labelled with its title; `Err` says how
    /// to resume it by hand.
    fn resume(&self, pane: Result<HostPane, HostError>, session: &Resuming) -> Result<(), String> {
        let command = resume_command_line(&session.id.to_string());
        let label = Some(session.title.as_str()).filter(|title| !title.is_empty());
        pane.and_then(|pane| self.host.run(&pane, &command, label))
            .map_err(|error| {
                format!(
                    "{} {PANE_FAILED}: {error}. Resume it there with: {command}",
                    self.host.host()
                )
            })
    }

    /// Starts the removal in a process group of its own, which outlives this
    /// pane when the host closes it.
    fn start_removal(&self) -> String {
        let name = &self.retiring.name;
        let mut command = self
            .host
            .remove_command(self.location, self.retiring.removal.force);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        match command.spawn() {
            Ok(child) => {
                drop(child);
                format!("Removing worktree {name}; its branch is kept")
            }
            Err(error) => format!("{REMOVE_FAILED} {name}: {error}"),
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

fn count(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        count => format!("{count} {noun}s"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    use caudra_agent::worktree::{
        Backend, Changes, CloseRequest, CreateRequest, Removal, RemoveRequest, Request,
        checkout_path,
    };
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{SessionDatabase, SessionLease};
    use caudra_terminal_host::resume::resume_command_line;
    use caudra_terminal_host::workspaces::{HostLocation, HostPane, Opened, Workspaces};
    use caudra_ui::{AppSession, SessionTab};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CHANGES_KEPT, CLOSE_FAILED, CREATE_FAILED, Executed, HandOff, Handled, Leaving,
        PANE_FAILED, REMOVAL_KEPT, Retiring, execute, relocate, relocation,
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
    const MAIN_WORKSPACE: &str = "w1";
    const NEW_TAB: &str = "w1:t9";
    const NEW_PANE: &str = "w1:p9";
    const NOTES: &str = "notes.txt";
    const EDITED: &str = "edited\n";
    const TITLES: [&str; 2] = ["Working in the worktree", "Working in main"];
    /// A Herdr that opens every tab as the same pane in the main workspace.
    const HERDR_OPENS_TABS: &str = r#"case "$1 $2" in
"tab create") echo '{"id":"cli","result":{"type":"tab_created","tab":{"tab_id":"w1:t9"},"root_pane":{"pane_id":"w1:p9"}}}' ;;
"pane get") echo '{"id":"cli","result":{"type":"pane_info","pane":{"pane_id":"w1:p9","tab_id":"w1:t9","workspace_id":"w1"}}}' ;;
esac"#;

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
                backend: Backend::git(Some(base.join(WORKTREES))),
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

        fn close(&self, tabs: Vec<SessionTab>, root: &Path) -> Executed {
            let request = CloseRequest {
                root: root.to_path_buf(),
                main_root: self.root.clone(),
                branch: Some(BRANCH.into()),
            };
            execute(
                Request::Close(request),
                tabs,
                0,
                &self.storage,
                &self.backend,
            )
        }

        fn head(&self, dir: &Path) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
        fn herdr(&self, outcome: &str) -> Arc<dyn Workspaces> {
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
            let socket = self.base.join("herdr.sock");
            caudra_terminal_host::detect_from(|name| match name {
                "HERDR_ENV" => Some("1".into()),
                "HERDR_PANE_ID" => Some(PANE.into()),
                "HERDR_SOCKET_PATH" => Some(socket.clone().into()),
                "HERDR_BIN_PATH" => Some(OsString::from(&binary)),
                _ => None,
            })
            .and_then(|host| host.workspaces())
            .unwrap()
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
            let opened = Opened {
                location: HostLocation(WORKSPACE.into()),
                root: worktree.clone(),
                branch: Some(BRANCH.into()),
                pane: HostPane::Open {
                    pane: PANE.into(),
                    tab: Some(TAB.into()),
                },
                already_open: false,
            };
            let hand_off = HandOff {
                id: moved,
                relocation: &request,
                opened: &opened,
                name: LABEL,
                host: herdr.as_ref(),
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

    fn commit(dir: &Path, message: &str) {
        git(
            dir,
            &["commit", "--quiet", "--allow-empty", "--message", message],
        );
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
                assert!(notes.iter().any(|note| note.contains(PANE_FAILED)));
            }
            Handled::Relocate { .. } => panic!("Herdr hand-offs never relocate this process"),
        }
        assert_eq!(repository.stored_cwd(moved), worktree.to_string_lossy());
        assert!(repository.herdr_calls().iter().any(|call| call == "run"));
    }

    #[test_case(false ; "clean")]
    #[test_case(true ; "dirty")]
    fn closing_a_git_worktree_brings_its_work_back_before_it_goes(dirty: bool) {
        let repository = Repository::new();
        let worktree = repository.worktree();
        commit(&worktree, "work");
        if dirty {
            fs::write(worktree.join(NOTES), EDITED).unwrap();
        }
        let tab = repository.tab(&worktree);
        let id = tab.session.id;

        let Executed { handled, .. } = repository.close(vec![tab], &worktree);
        let Handled::Relocate {
            handoff, follow_up, ..
        } = handled
        else {
            panic!("the session working in the worktree moves out first");
        };
        assert_eq!(
            repository.head(&repository.root),
            repository.head(&worktree)
        );
        relocate(&repository.storage, &handoff.request).unwrap();
        follow_up.finish(Vec::new(), true, &repository.backend, &repository.storage);

        assert!(!worktree.exists());
        assert_eq!(repository.stored_cwd(id), repository.root.to_string_lossy());
        assert_eq!(repository.root.join(NOTES).exists(), dirty);
    }

    #[test]
    fn a_worktree_that_cannot_fast_forward_main_is_left_alone() {
        let repository = Repository::new();
        let worktree = repository.worktree();
        commit(&worktree, "branch");
        commit(&repository.root, "main");
        let main_head = repository.head(&repository.root);
        let tab = repository.tab(&worktree);
        let id = tab.session.id;

        let Executed { handled, notes } = repository.close(vec![tab], &worktree);

        assert!(matches!(handled, Handled::Continue { .. }));
        assert!(
            notes.iter().any(|note| note.starts_with(CLOSE_FAILED)),
            "{notes:?}"
        );
        assert!(worktree.exists());
        assert_eq!(repository.head(&repository.root), main_head);
        assert_eq!(repository.stored_cwd(id), worktree.to_string_lossy());
    }

    #[test]
    fn changes_that_cannot_come_back_keep_the_worktree() {
        let repository = Repository::new();
        let worktree = repository.worktree();
        fs::write(worktree.join(NOTES), EDITED).unwrap();
        let tab = repository.tab(&worktree);

        let Executed { handled, .. } = repository.close(vec![tab], &worktree);
        let Handled::Relocate {
            handoff, follow_up, ..
        } = handled
        else {
            panic!("the session working in the worktree moves out first");
        };
        relocate(&repository.storage, &handoff.request).unwrap();
        let lock = repository.root.join(".git").join("index.lock");
        fs::write(&lock, "").unwrap();
        let notes = follow_up.finish(Vec::new(), true, &repository.backend, &repository.storage);
        fs::remove_file(lock).unwrap();

        assert!(
            notes.iter().any(|note| note.starts_with(CHANGES_KEPT)),
            "{notes:?}"
        );
        assert!(worktree.join(NOTES).exists());
        assert!(!repository.root.join(NOTES).exists());
    }

    /// Every session resumes in a tab of its own in the repository's
    /// workspace, the focused one last so it keeps the focus.
    #[test]
    fn leaving_a_herdr_workspace_resumes_every_session_in_a_tab_of_its_own() {
        let repository = Repository::new();
        let worktree = repository.worktree();
        let mut tabs = vec![repository.tab(&worktree), repository.tab(&repository.root)];
        for (tab, title) in tabs.iter_mut().zip(TITLES) {
            tab.session.title = title.into();
        }
        let [working, staying] = [tabs[0].session.id, tabs[1].session.id];
        let herdr = repository.herdr(HERDR_OPENS_TABS);
        let location = HostLocation(WORKSPACE.into());
        let retiring = Retiring {
            removal: Removal {
                root: worktree.clone(),
                main_root: repository.root.clone(),
                force: false,
                location: Some(location.clone()),
                main_location: Some(HostLocation(MAIN_WORKSPACE.into())),
            },
            name: LABEL.into(),
            carry_back: false,
        };
        let leaving = Leaving {
            host: herdr.as_ref(),
            retiring: &retiring,
            location: &location,
            destination: &repository.root,
        };

        let Executed { handled, notes } =
            leaving.run(tabs, 0, &[working], &repository.storage, Vec::new());

        assert!(matches!(handled, Handled::Exit), "{notes:?}");
        assert_eq!(
            repository.stored_cwd(working),
            repository.root.to_string_lossy()
        );
        let main = repository.root.to_string_lossy().into_owned();
        let [focused_title, other_title] = TITLES;
        let (focused_resume, other_resume) = (
            resume_command_line(&working.to_string()),
            resume_command_line(&staying.to_string()),
        );
        let tab_create = |workspace: &'static str| {
            [
                "tab",
                "create",
                "--workspace",
                workspace,
                "--cwd",
                main.as_str(),
                "--focus",
            ]
            .map(str::to_owned)
        };
        let resume = |title: &str, command: &str| {
            [
                "tab", "rename", NEW_TAB, title, "pane", "rename", NEW_PANE, title, "pane", "run",
                NEW_PANE, command,
            ]
            .map(str::to_owned)
        };
        let expected: Vec<String> = tab_create(MAIN_WORKSPACE)
            .into_iter()
            .chain(resume(other_title, &other_resume))
            .chain(["pane", "get", NEW_PANE].map(str::to_owned))
            .chain(tab_create(MAIN_WORKSPACE))
            .chain(resume(focused_title, &focused_resume))
            .collect();
        let calls = repository.herdr_calls();
        assert_eq!(&calls[..expected.len().min(calls.len())], &expected[..]);
    }
}
