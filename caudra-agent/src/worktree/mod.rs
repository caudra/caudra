//! Git worktrees `/worktree` creates and removes: through Herdr when Caudra
//! runs in one of its panes, so each lands in a workspace of its own, and
//! through git otherwise.

pub mod git;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_config::{WorktreeBackend, WorktreesConfig};
use caudra_storage::id::CaudraId;
use caudra_storage::{paths, random_task_id};
use thiserror::Error;
use tracing::warn;

use crate::herdr::{HerdrEnv, HerdrError, NOT_LINKED_WORKTREE};
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

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Herdr(#[from] HerdrError),
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
}

#[derive(Clone, Debug)]
pub enum Backend {
    /// Herdr creates, groups and removes worktrees, and a session moved into
    /// one continues in the new workspace's pane.
    Herdr(HerdrEnv),
    /// Git does, and this process follows the session. `directory` is where
    /// checkouts go when configured, else under the data directory.
    Git { directory: Option<PathBuf> },
}

impl Backend {
    pub fn select(config: &WorktreesConfig, herdr: Option<HerdrEnv>) -> Self {
        match (config.backend.unwrap_or_default(), herdr) {
            (WorktreeBackend::Auto, Some(herdr)) => Self::Herdr(herdr),
            _ => Self::Git {
                directory: config.directory.as_deref().map(expand_home),
            },
        }
    }

    pub fn herdr(&self) -> Option<&HerdrEnv> {
        match self {
            Self::Herdr(herdr) => Some(herdr),
            Self::Git { .. } => None,
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Create(CreateRequest),
    Remove(RemoveRequest),
}

/// A worktree [`create`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// The Herdr pane the session continues in.
    pub pane: Option<String>,
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
    created.carry_failed = stash.and_then(|stash| carry(&source, &created.root, &stash));
    Ok(created)
}

fn add(
    backend: &Backend,
    request: &CreateRequest,
    branch: Option<String>,
    base: &str,
) -> Result<Created, WorktreeError> {
    match backend {
        Backend::Herdr(herdr) => {
            let opened =
                herdr
                    .cli()
                    .worktree_create(&request.main_root, branch.as_deref(), base)?;
            Ok(Created {
                root: canonical(opened.worktree.path)?,
                branch: opened.worktree.branch,
                pane: Some(opened.workspace.pane_id),
                carry_failed: None,
            })
        }
        Backend::Git { directory } => {
            let branch = match branch {
                Some(branch) => branch,
                None => generated_branch().map_err(WorktreeError::BranchName)?,
            };
            let directory = checkout_directory(directory.as_deref()).map_err(|source| {
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
                pane: None,
                carry_failed: None,
            })
        }
    }
}

/// Applies `stash` in `root` and drops it, or puts it back into the source
/// when it will not apply, returning why it did not come along.
fn carry(source: &Git<'_>, root: &Path, stash: &str) -> Option<String> {
    let error = match Git::new(root).stash_apply(stash) {
        Ok(()) => {
            return source
                .stash_drop(stash)
                .err()
                .map(|error| format!("they also stay stashed as {stash}: {error}"));
        }
        Err(error) => error,
    };
    Some(match restore(source, stash) {
        Ok(()) => format!("they stay in the source checkout: {error}"),
        Err(_) => format!("they stay stashed as {stash}: {error}"),
    })
}

fn restore(source: &Git<'_>, stash: &str) -> Result<(), GitError> {
    source.stash_apply(stash)?;
    source.stash_drop(stash)
}

/// How a checkout goes: through Herdr when it has the checkout open as a
/// worktree workspace, which it then closes, else through git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub root: PathBuf,
    pub main_root: PathBuf,
    pub force: bool,
    /// The Herdr workspace the checkout is open in.
    pub workspace: Option<String>,
    /// The Herdr workspace the repository's own checkout is open in.
    pub source_workspace: Option<String>,
}

impl Removal {
    /// Stashes the checkout's changes when `request` asks to, returning the
    /// stash, and finds where Herdr has it open.
    pub fn prepare(
        backend: &Backend,
        request: &RemoveRequest,
    ) -> Result<(Self, Option<String>), WorktreeError> {
        let stash = match request.changes {
            Changes::Stash => Git::new(&request.root).stash_push(REMOVAL_MESSAGE)?,
            Changes::None | Changes::Discard => None,
        };
        let listing = backend.herdr().and_then(|herdr| {
            herdr
                .cli()
                .worktree_list(&request.main_root)
                .inspect_err(|error| warn!(%error, "Herdr did not list the worktrees"))
                .ok()
        });
        let removal = Self {
            root: request.root.clone(),
            main_root: request.main_root.clone(),
            force: request.changes == Changes::Discard,
            workspace: listing
                .as_ref()
                .and_then(|listing| listing.workspace_of(&request.root))
                .map(str::to_owned),
            source_workspace: listing.and_then(|listing| listing.source.source_workspace_id),
        };
        Ok((removal, stash))
    }

    /// Removes the checkout, keeping its branch.
    pub fn run(&self, backend: &Backend) -> Result<(), WorktreeError> {
        if let (Some(herdr), Some(workspace)) = (backend.herdr(), &self.workspace) {
            match herdr.cli().worktree_remove(workspace, self.force) {
                Ok(()) => return Ok(()),
                Err(error) if error.code() == Some(NOT_LINKED_WORKTREE) => {}
                Err(error) => return Err(error.into()),
            }
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
        Backend, Changes, CreateRequest, Removal, RemoveRequest, WorktreeError, checkout_path,
        counterpart, create, label, slug,
    };

    const DIRECTORY: &str = "/data/worktrees";
    const MAIN_ROOT: &str = "/work/app";
    const BRANCH: &str = "feature/login";
    const WORKTREES: &str = "worktrees";

    fn git_backend(repository: &Repository) -> Backend {
        Backend::Git {
            directory: Some(repository.sibling(WORKTREES)),
        }
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
