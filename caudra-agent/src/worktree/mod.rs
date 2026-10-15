//! Git worktrees `/worktree` creates and removes: through the terminal host
//! Caudra runs in when it manages worktrees, so each lands in a workspace of
//! its own, and through git otherwise.

pub mod git;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_config::{WorktreeBackend, WorktreesConfig};
use caudra_storage::id::CaudraId;
use caudra_storage::{paths, random_task_id};
use caudra_terminal_host::HostError;
use caudra_terminal_host::workspaces::{HostLocation, Opened, Workspaces};
use thiserror::Error;
use tracing::warn;

use git::{Git, GitError};

/// Starts every branch Caudra names itself, so its worktrees stand out.
pub const GENERATED_BRANCH_PREFIX: &str = "caudra/";
const WORKTREES_DIR: &str = "worktrees";
const HOME_PREFIX: &str = "~/";
const SLUG_SEPARATOR: char = '-';
const SLUG_KEPT: [char; 3] = ['-', '_', '.'];
const UNNAMED_REPOSITORY: &str = "repository";
const CARRY_MESSAGE: &str = "caudra: carried to a new worktree";
const REMOVAL_MESSAGE: &str = "caudra: left by removed worktree";
const CLOSE_MESSAGE: &str = "caudra: carried back from a closed worktree";
const CHANGES_STAYED: &str = "they stay in the checkout they came from";

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("cannot prepare {}: {source}", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} already exists", .0.display())]
    Exists(PathBuf),
    #[error("cannot name a branch: {0}")]
    BranchName(String),
    #[error("uncommitted changes carry over only to a worktree that starts at HEAD")]
    CarryNeedsHead,
    #[error(
        "the main checkout has commits this worktree does not; merge or rebase its branch first"
    )]
    Diverged,
    #[error(
        "the main checkout has uncommitted changes of its own; commit or stash them first, so the worktree's can come back"
    )]
    MainDirty,
    #[error(
        "the main checkout has moved past this worktree's commit, so its uncommitted changes have no commit to come back onto; commit them first"
    )]
    MovedPast,
}

/// Who creates and removes worktrees: the terminal host when there is one
/// that manages them, and a session moved into one continues in the host's
/// pane; else git, and this process follows the session. Git also takes
/// over a repository the host does not manage.
#[derive(Clone, Debug)]
pub struct Backend {
    host: Option<Arc<dyn Workspaces>>,
    /// Where git puts checkouts when configured, else under the data
    /// directory.
    directory: Option<PathBuf>,
}

impl Backend {
    pub fn select(config: &WorktreesConfig, host: Option<Arc<dyn Workspaces>>) -> Self {
        Self {
            host: host.filter(|_| config.backend.unwrap_or_default() == WorktreeBackend::Auto),
            directory: config.directory.as_deref().map(expand_home),
        }
    }

    pub fn git(directory: Option<PathBuf>) -> Self {
        Self {
            host: None,
            directory,
        }
    }

    pub fn with_host(self, host: Arc<dyn Workspaces>) -> Self {
        Self {
            host: Some(host),
            ..self
        }
    }

    pub fn host(&self) -> Option<&dyn Workspaces> {
        self.host.as_deref()
    }
}

/// Creates a worktree and moves the focused session into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateRequest {
    pub session: CaudraId,
    /// Where the session works, inside `source`.
    pub cwd: PathBuf,
    /// The root of the checkout the worktree branches off.
    pub source: PathBuf,
    pub main_root: PathBuf,
    /// `None` lets the backend name it.
    pub branch: Option<String>,
    /// A revision of `source`.
    pub base: String,
    /// Moves the source's uncommitted changes along.
    pub carry: bool,
}

/// What happens to a removed worktree's uncommitted changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changes {
    /// It has none.
    None,
    /// Stashed, where every checkout of the repository can apply them.
    Stash,
    Discard,
}

/// Removes a linked worktree, keeping its branch, and moves its sessions
/// back to a checkout that remains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveRequest {
    pub root: PathBuf,
    pub main_root: PathBuf,
    pub branch: Option<String>,
    pub changes: Changes,
}

/// Brings a linked worktree's commits and uncommitted changes back to the
/// main checkout, moves its sessions there, and removes it, keeping its
/// branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseRequest {
    pub root: PathBuf,
    pub main_root: PathBuf,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Create(CreateRequest),
    Remove(RemoveRequest),
    Close(CloseRequest),
}

/// What closing a worktree brings back to the main checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosePlan {
    /// The worktree's commit.
    pub head: String,
    /// How many commits the main checkout fast-forwards by to reach `head`.
    pub commits: usize,
    /// Whether the worktree has uncommitted changes to carry back.
    pub carries: bool,
}

/// What closing the worktree at `root` would bring back, refused when the
/// main checkout cannot take it: only a fast-forward brings commits back,
/// and uncommitted changes come back only onto the worktree's own commit in
/// a clean main checkout, where they always apply. Changes nothing.
pub fn plan_close(main_root: &Path, root: &Path) -> Result<ClosePlan, WorktreeError> {
    let main = Git::new(main_root);
    let worktree = Git::new(root);
    let head = worktree.head()?;
    let main_head = main.head()?;
    let carries = worktree.is_dirty()?;
    let commits = if main.is_ancestor(&main_head, &head)? {
        main.commits_between(&main_head, &head)?
    } else if !main.is_ancestor(&head, &main_head)? {
        return Err(WorktreeError::Diverged);
    } else if carries {
        return Err(WorktreeError::MovedPast);
    } else {
        0
    };
    if carries && main.is_dirty()? {
        return Err(WorktreeError::MainDirty);
    }
    Ok(ClosePlan {
        head,
        commits,
        carries,
    })
}

/// Fast-forwards the main checkout to the worktree's commit, when it is
/// behind it.
pub fn fast_forward_main(main_root: &Path, plan: &ClosePlan) -> Result<(), WorktreeError> {
    if plan.commits > 0 {
        Git::new(main_root).fast_forward(&plan.head)?;
    }
    Ok(())
}

/// Moves the uncommitted changes of the worktree at `root` into the main
/// checkout, which [`plan_close`] and [`fast_forward_main`] left clean at the
/// worktree's commit. `Ok` holds a note when they came back but stay stashed
/// too; `Err` says why they stayed in the worktree, which therefore has to
/// stay.
pub fn carry_back(root: &Path, main_root: &Path) -> Result<Option<String>, String> {
    let worktree = Git::new(root);
    match worktree.stash_push(CLOSE_MESSAGE) {
        Ok(Some(stash)) => carry(&worktree, main_root, &stash),
        Ok(None) => Ok(None),
        Err(error) => Err(format!("{CHANGES_STAYED}: {error}")),
    }
}

/// A worktree [`create`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// Where the host opened it, whose pane the session continues in.
    pub opened: Option<Opened>,
    /// Why git made it although there is a host.
    pub host_declined: Option<String>,
    /// Why the source's uncommitted changes did not come along.
    pub carry_failed: Option<String>,
}

/// Creates the worktree `request` asks for, carrying the source's
/// uncommitted changes when asked to. They carry only onto the commit they
/// were made on, where they always apply; should that still fail, they go
/// back to the source and the worktree stays.
pub fn create(backend: &Backend, request: &CreateRequest) -> Result<Created, WorktreeError> {
    let source = Git::new(&request.source);
    let base = source.commit(&request.base)?;
    if request.carry && base != source.head()? {
        return Err(WorktreeError::CarryNeedsHead);
    }
    let branch = request
        .branch
        .as_deref()
        .map(|name| source.branch_name(name))
        .transpose()?;
    let stash = if request.carry {
        source.stash_push(CARRY_MESSAGE)?
    } else {
        None
    };
    let mut created = match add(backend, request, branch, &base) {
        Ok(created) => created,
        Err(error) => {
            if let Some(stash) = &stash
                && let Err(restore_error) = restore(&source, stash)
            {
                warn!(%restore_error, stash, "carried changes stay stashed");
            }
            return Err(error);
        }
    };
    created.carry_failed =
        stash.and_then(|stash| carry(&source, &created.root, &stash).unwrap_or_else(Some));
    Ok(created)
}

fn add(
    backend: &Backend,
    request: &CreateRequest,
    branch: Option<String>,
    base: &str,
) -> Result<Created, WorktreeError> {
    let Some(host) = backend.host() else {
        return add_with_git(backend, request, branch, base);
    };
    let branch = match branch {
        None if !host.names_branches() => {
            Some(generated_branch().map_err(WorktreeError::BranchName)?)
        }
        branch => branch,
    };
    match host.create(&request.main_root, branch.as_deref(), base) {
        Ok(opened) => Ok(Created {
            root: canonical(opened.root.clone())?,
            branch: opened.branch.clone(),
            opened: Some(opened),
            host_declined: None,
            carry_failed: None,
        }),
        Err(error @ HostError::UnknownRepository { .. }) => {
            let mut created = add_with_git(backend, request, branch, base)?;
            created.host_declined = Some(error.to_string());
            Ok(created)
        }
        Err(error) => Err(error.into()),
    }
}

fn add_with_git(
    backend: &Backend,
    request: &CreateRequest,
    branch: Option<String>,
    base: &str,
) -> Result<Created, WorktreeError> {
    let branch = match branch {
        Some(branch) => branch,
        None => generated_branch().map_err(WorktreeError::BranchName)?,
    };
    let directory = checkout_directory(backend.directory.as_deref()).map_err(|source| {
        WorktreeError::Directory {
            path: PathBuf::from(WORKTREES_DIR),
            source,
        }
    })?;
    let path = checkout_path(&directory, &request.main_root, &branch);
    if path.exists() {
        return Err(WorktreeError::Exists(path));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| WorktreeError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    Git::new(&request.source).add_worktree(&path, &branch, base)?;
    Ok(Created {
        root: canonical(path)?,
        branch: Some(branch),
        opened: None,
        host_declined: None,
        carry_failed: None,
    })
}

/// Applies `stash`, taken in `source`, in `root` and drops it, saying so when
/// it could not be dropped. When it will not apply it goes back into the
/// source, and the error says why the changes did not come along.
fn carry(source: &Git<'_>, root: &Path, stash: &str) -> Result<Option<String>, String> {
    let error = match Git::new(root).stash_apply(stash) {
        Ok(()) => {
            return Ok(source
                .stash_drop(stash)
                .err()
                .map(|error| format!("they also stay stashed as {stash}: {error}")));
        }
        Err(error) => error,
    };
    Err(match restore(source, stash) {
        Ok(()) => format!("{CHANGES_STAYED}: {error}"),
        Err(_) => format!("they stay stashed as {stash}: {error}"),
    })
}

fn restore(source: &Git<'_>, stash: &str) -> Result<(), GitError> {
    source.stash_apply(stash)?;
    source.stash_drop(stash)
}

/// How a checkout goes: through the host when it has the checkout open as a
/// worktree, which it then closes, else through git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub root: PathBuf,
    pub main_root: PathBuf,
    pub force: bool,
    /// Where the host has the checkout open.
    pub location: Option<HostLocation>,
    /// Where the host has the repository's own checkout open.
    pub main_location: Option<HostLocation>,
}

impl Removal {
    /// Stashes the checkout's changes when `request` asks to, returning the
    /// stash, and finds where the host has it open.
    pub fn prepare(
        backend: &Backend,
        request: &RemoveRequest,
    ) -> Result<(Self, Option<String>), WorktreeError> {
        let stash = match request.changes {
            Changes::Stash => Git::new(&request.root).stash_push(REMOVAL_MESSAGE)?,
            Changes::None | Changes::Discard => None,
        };
        let located = backend
            .host()
            .and_then(|host| {
                host.locate(&request.main_root, &request.root)
                    .inspect_err(|error| warn!(%error, host = host.host(), "the host did not list the worktrees"))
                    .ok()
            })
            .unwrap_or_default();
        let removal = Self {
            root: request.root.clone(),
            main_root: request.main_root.clone(),
            force: request.changes == Changes::Discard,
            location: located.checkout,
            main_location: located.main,
        };
        Ok((removal, stash))
    }

    /// Removes the checkout, keeping its branch.
    pub fn run(&self, backend: &Backend) -> Result<(), WorktreeError> {
        if let (Some(host), Some(location)) = (backend.host(), &self.location)
            && host.remove(location, self.force)?
        {
            return Ok(());
        }
        Ok(Git::new(&self.main_root).remove_worktree(&self.root, self.force)?)
    }
}

fn canonical(path: PathBuf) -> Result<PathBuf, WorktreeError> {
    path.canonicalize()
        .map_err(|source| WorktreeError::Directory { path, source })
}

/// Where git puts a new checkout of `main_root`'s repository on `branch`:
/// `<directory>/<repository>/<branch>`, the branch made one path component.
pub fn checkout_path(directory: &Path, main_root: &Path, branch: &str) -> PathBuf {
    let repository = main_root
        .file_name()
        .map_or_else(|| UNNAMED_REPOSITORY.into(), |name| name.to_string_lossy());
    directory.join(repository.as_ref()).join(slug(branch))
}

/// The configured directory for git-created checkouts, else one under the
/// data directory.
pub fn checkout_directory(configured: Option<&Path>) -> Result<PathBuf, io::Error> {
    match configured {
        Some(directory) => Ok(directory.to_path_buf()),
        None => Ok(paths::data_dir()?.join(WORKTREES_DIR)),
    }
}

/// A branch name for a worktree the user did not name.
pub fn generated_branch() -> Result<String, String> {
    random_task_id()
        .map(|phrase| format!("{GENERATED_BRANCH_PREFIX}{phrase}"))
        .map_err(|error| error.to_string())
}

/// `cwd`'s counterpart below `to` for a path below `from`: the same relative
/// directory when `to` has it inside itself, else `to`. Canonical when `to`
/// is.
pub fn counterpart(cwd: &Path, from: &Path, to: &Path) -> PathBuf {
    cwd.strip_prefix(from)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .and_then(|relative| to.join(relative).canonicalize().ok())
        .filter(|nested| nested.is_dir() && nested.starts_with(to))
        .unwrap_or_else(|| to.to_path_buf())
}

/// How a checkout is named to the user: its branch, else its directory.
pub fn label(branch: Option<&str>, root: &Path) -> String {
    branch.map(str::to_owned).unwrap_or_else(|| {
        root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    })
}

fn slug(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len());
    for character in branch.chars() {
        if character.is_alphanumeric() || SLUG_KEPT.contains(&character) {
            slug.push(character);
        } else if !slug.ends_with(SLUG_SEPARATOR) {
            slug.push(SLUG_SEPARATOR);
        }
    }
    slug.trim_matches(|character| character == SLUG_SEPARATOR || character == '.')
        .to_owned()
}

fn expand_home(directory: &str) -> PathBuf {
    match (directory.strip_prefix(HOME_PREFIX), paths::home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(directory),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use caudra_storage::id::CaudraId;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::git::Git;
    use super::git::fixture::{EDITED, ORIGINAL, Repository, TRACKED, UNTRACKED, git, stashes};
    use super::{
        Backend, Changes, CreateRequest, Removal, RemoveRequest, WorktreeError, carry_back,
        checkout_path, counterpart, create, fast_forward_main, label, plan_close, slug,
    };

    const DIRECTORY: &str = "/data/worktrees";
    const MAIN_ROOT: &str = "/work/app";
    const BRANCH: &str = "feature/login";
    const WORKTREES: &str = "worktrees";

    fn git_backend(repository: &Repository) -> Backend {
        Backend::git(Some(repository.sibling(WORKTREES)))
    }

    fn create_request(repository: &Repository, base: &str, carry: bool) -> CreateRequest {
        CreateRequest {
            session: CaudraId::generate(),
            cwd: repository.root.clone(),
            source: repository.root.clone(),
            main_root: repository.root.clone(),
            branch: Some(BRANCH.into()),
            base: base.into(),
            carry,
        }
    }

    #[test]
    fn a_new_worktree_takes_the_uncommitted_changes_along() {
        let repository = Repository::new();
        repository.edit(&repository.root);

        let created = create(
            &git_backend(&repository),
            &create_request(&repository, "HEAD", true),
        )
        .unwrap();

        assert_eq!(
            created.root,
            repository
                .sibling(WORKTREES)
                .join("main")
                .join("feature-login")
        );
        assert_eq!(created.branch.as_deref(), Some(BRANCH));
        assert_eq!(created.carry_failed, None);
        assert_eq!(
            fs::read_to_string(created.root.join(TRACKED)).unwrap(),
            EDITED
        );
        assert_eq!(
            fs::read_to_string(created.root.join(UNTRACKED)).unwrap(),
            EDITED
        );
        assert!(!repository.git().is_dirty().unwrap());
        assert_eq!(stashes(&repository.root), 0);
    }

    #[test]
    fn changes_do_not_carry_onto_another_commit() {
        let repository = Repository::new();
        git(
            &repository.root,
            &["commit", "--quiet", "--allow-empty", "--message", "later"],
        );
        repository.edit(&repository.root);

        let error = create(
            &git_backend(&repository),
            &create_request(&repository, "HEAD~1", true),
        )
        .unwrap_err();

        assert!(matches!(error, WorktreeError::CarryNeedsHead));
        assert!(repository.git().is_dirty().unwrap());
        assert!(!repository.sibling(WORKTREES).exists());
    }

    #[test]
    fn an_existing_directory_is_never_reused() {
        let repository = Repository::new();
        let taken = repository
            .sibling(WORKTREES)
            .join("main")
            .join("feature-login");
        fs::create_dir_all(&taken).unwrap();

        let error = create(
            &git_backend(&repository),
            &create_request(&repository, "HEAD", false),
        )
        .unwrap_err();

        assert!(matches!(error, WorktreeError::Exists(path) if path == taken));
    }

    #[test_case(Changes::Stash ; "stashed")]
    #[test_case(Changes::Discard ; "discarded")]
    fn a_dirty_worktree_is_removed_as_asked_and_keeps_its_branch(changes: Changes) {
        let repository = Repository::new();
        let backend = git_backend(&repository);
        let created = create(&backend, &create_request(&repository, "HEAD", false)).unwrap();
        repository.edit(&created.root);

        let (removal, stash) = Removal::prepare(
            &backend,
            &RemoveRequest {
                root: created.root.clone(),
                main_root: repository.root.clone(),
                branch: created.branch.clone(),
                changes,
            },
        )
        .unwrap();
        removal.run(&backend).unwrap();

        assert!(!created.root.exists());
        assert!(repository.git().has_branch(BRANCH).unwrap());
        assert_eq!(removal.force, changes == Changes::Discard);
        assert_eq!(stash.is_some(), changes == Changes::Stash);
        if let Some(stash) = stash {
            repository.git().stash_apply(&stash).unwrap();
            assert_eq!(
                fs::read_to_string(repository.root.join(TRACKED)).unwrap(),
                EDITED
            );
        } else {
            assert_eq!(
                fs::read_to_string(repository.root.join(TRACKED)).unwrap(),
                ORIGINAL
            );
        }
    }

    #[test]
    fn a_clean_removal_refuses_changes_made_since() {
        let repository = Repository::new();
        let backend = git_backend(&repository);
        let created = create(&backend, &create_request(&repository, "HEAD", false)).unwrap();
        let (removal, _) = Removal::prepare(
            &backend,
            &RemoveRequest {
                root: created.root.clone(),
                main_root: repository.root.clone(),
                branch: created.branch.clone(),
                changes: Changes::None,
            },
        )
        .unwrap();
        repository.edit(&created.root);

        assert!(matches!(removal.run(&backend), Err(WorktreeError::Git(_))));
        assert!(Git::new(&created.root).is_dirty().unwrap());
    }

    fn commit(root: &Path, message: &str) {
        git(
            root,
            &["commit", "--quiet", "--allow-empty", "--message", message],
        );
    }

    fn linked(repository: &Repository) -> PathBuf {
        create(
            &git_backend(repository),
            &create_request(repository, "HEAD", false),
        )
        .unwrap()
        .root
    }

    #[test_case(false ; "clean")]
    #[test_case(true ; "dirty")]
    fn closing_brings_commits_and_changes_back(dirty: bool) {
        let repository = Repository::new();
        let root = linked(&repository);
        commit(&root, "work");
        if dirty {
            repository.edit(&root);
        }

        let plan = plan_close(&repository.root, &root).unwrap();
        fast_forward_main(&repository.root, &plan).unwrap();
        let carried = carry_back(&root, &repository.root);

        assert_eq!(plan.commits, 1);
        assert_eq!(plan.carries, dirty);
        assert_eq!(carried, Ok(None));
        assert_eq!(repository.git().head().unwrap(), plan.head);
        assert!(!Git::new(&root).is_dirty().unwrap());
        assert_eq!(repository.git().is_dirty().unwrap(), dirty);
        let expected = if dirty { EDITED } else { ORIGINAL };
        assert_eq!(
            fs::read_to_string(repository.root.join(TRACKED)).unwrap(),
            expected
        );
        assert_eq!(stashes(&repository.root), 0);
    }

    #[test_case(true, true, false, false, WorktreeError::Diverged ; "diverged")]
    #[test_case(false, false, true, true, WorktreeError::MainDirty ; "main_dirty")]
    #[test_case(true, false, true, false, WorktreeError::MovedPast ; "moved_past_with_changes")]
    fn closing_is_refused_without_touching_either_checkout(
        main_commits: bool,
        branch_commits: bool,
        worktree_dirty: bool,
        main_dirty: bool,
        expected: WorktreeError,
    ) {
        let repository = Repository::new();
        let root = linked(&repository);
        if main_commits {
            commit(&repository.root, "main");
        }
        if branch_commits {
            commit(&root, "branch");
        }
        if worktree_dirty {
            repository.edit(&root);
        }
        if main_dirty {
            fs::write(repository.root.join(UNTRACKED), ORIGINAL).unwrap();
        }
        let main_head = repository.git().head().unwrap();

        let error = plan_close(&repository.root, &root).unwrap_err();

        assert_eq!(error.to_string(), expected.to_string());
        assert_eq!(repository.git().head().unwrap(), main_head);
        assert_eq!(Git::new(&root).is_dirty().unwrap(), worktree_dirty);
    }

    #[test]
    fn a_clean_worktree_the_main_checkout_moved_past_closes_without_a_merge() {
        let repository = Repository::new();
        let root = linked(&repository);
        commit(&repository.root, "main");
        let main_head = repository.git().head().unwrap();

        let plan = plan_close(&repository.root, &root).unwrap();
        fast_forward_main(&repository.root, &plan).unwrap();

        assert_eq!(plan.commits, 0);
        assert_eq!(repository.git().head().unwrap(), main_head);
    }

    #[test]
    fn changes_that_will_not_apply_stay_in_the_worktree() {
        let repository = Repository::new();
        let root = linked(&repository);
        repository.edit(&root);
        plan_close(&repository.root, &root).unwrap();
        let lock = repository.root.join(".git").join("index.lock");
        fs::write(&lock, "").unwrap();

        let carried = carry_back(&root, &repository.root);
        fs::remove_file(lock).unwrap();

        assert!(carried.is_err(), "{carried:?}");
        assert_eq!(fs::read_to_string(root.join(TRACKED)).unwrap(), EDITED);
        assert_eq!(fs::read_to_string(root.join(UNTRACKED)).unwrap(), EDITED);
        assert_eq!(
            fs::read_to_string(repository.root.join(TRACKED)).unwrap(),
            ORIGINAL
        );
        assert_eq!(stashes(&repository.root), 0);
    }

    #[test_case("feature/login", "feature-login" ; "nested_branch")]
    #[test_case("fix//double", "fix-double" ; "repeated_separator")]
    #[test_case("release/v1.2", "release-v1.2" ; "dotted_version")]
    #[test_case("./hidden/", "hidden" ; "trimmed_edges")]
    fn a_branch_becomes_one_path_component(branch: &str, expected: &str) {
        assert_eq!(slug(branch), expected);
    }

    #[test]
    fn a_checkout_lands_under_its_repository() {
        assert_eq!(
            checkout_path(Path::new(DIRECTORY), Path::new(MAIN_ROOT), "feature/login"),
            PathBuf::from("/data/worktrees/app/feature-login")
        );
    }

    #[test_case("src", true, "src" ; "same_directory_when_it_exists")]
    #[test_case("docs", false, "" ; "root_when_it_does_not")]
    #[test_case("", true, "" ; "root_from_the_root")]
    fn the_counterpart_keeps_the_relative_directory(relative: &str, exists: bool, expected: &str) {
        let temp = TempDir::new().unwrap();
        let from = temp.path().canonicalize().unwrap().join("from");
        let to = temp.path().canonicalize().unwrap().join("to");
        fs::create_dir_all(from.join(relative)).unwrap();
        fs::create_dir_all(&to).unwrap();
        if exists {
            fs::create_dir_all(to.join(relative)).unwrap();
        }

        assert_eq!(
            counterpart(&from.join(relative), &from, &to),
            to.join(expected)
        );
    }

    #[test_case(Some("feature/login"), "feature/login" ; "branch")]
    #[test_case(None, "app" ; "detached")]
    fn a_checkout_is_named_by_its_branch(branch: Option<&str>, expected: &str) {
        assert_eq!(label(branch, Path::new(MAIN_ROOT)), expected);
    }
}
