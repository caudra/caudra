//! Which checkout of which repository a directory is, read from the files git
//! keeps. Nothing here runs git.
//!
//! A linked worktree counts as one only when git's records agree from both
//! ends: its `.git` file names an admin directory inside a repository, and
//! that admin directory names the checkout back. A directory can write what it
//! likes into its own `.git` file but not into another repository's admin
//! directories, so a crafted checkout cannot claim a repository, and with it
//! that repository's state and trust.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::iter;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::normalize_path;
use crate::projects::GIT_MARKER;

const GITDIR_PREFIX: &str = "gitdir: ";
const BRANCH_REF_PREFIX: &str = "ref: refs/heads/";
const HEAD_FILE: &str = "HEAD";
const COMMONDIR_FILE: &str = "commondir";
const GITDIR_FILE: &str = "gitdir";
const WORKTREES_DIR: &str = "worktrees";

/// A repository's identity: the canonical git common directory every checkout
/// of it shares.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryKey(PathBuf);

impl RepositoryKey {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for RepositoryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The canonical top level of this checkout.
    pub root: PathBuf,
    pub repository: RepositoryKey,
    /// The main checkout's root, or the repository itself when it is bare.
    pub main_root: PathBuf,
    /// The admin directory name of a verified linked worktree.
    pub linked: Option<String>,
    pub branch: Option<String>,
}

impl Checkout {
    pub fn is_linked(&self) -> bool {
        self.linked.is_some()
    }

    pub fn is_bare(&self) -> bool {
        self.main_root == self.repository.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutMember {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// `None` for the main checkout.
    pub admin: Option<String>,
    /// Git still records the checkout, but it is gone from disk.
    pub prunable: bool,
}

/// Where a project's trust grant was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustSource {
    Exact,
    /// Granted to the same place in another checkout of the repository.
    Inherited(PathBuf),
}

impl TrustSource {
    /// Asks `granted` about `project`, then about its counterpart in each
    /// sibling checkout, stopping at the first grant.
    pub(crate) fn find<E>(
        project: &Path,
        mut granted: impl FnMut(&Path) -> Result<bool, E>,
    ) -> Result<Option<Self>, E> {
        if granted(project)? {
            return Ok(Some(Self::Exact));
        }
        for sibling in sibling_projects(project) {
            if granted(&sibling)? {
                return Ok(Some(Self::Inherited(sibling)));
            }
        }
        Ok(None)
    }
}

/// The checkout `cwd` is in, found at the nearest `.git` above it, or `None`
/// outside any. A `.git` file that does not verify as a linked worktree, such
/// as a submodule's, makes a checkout of its own.
pub fn discover(cwd: &Path) -> Option<Checkout> {
    let cwd = cwd.canonicalize().ok()?;
    let root = cwd.ancestors().find(|dir| dir.join(GIT_MARKER).exists())?;
    let marker = root.join(GIT_MARKER).canonicalize().ok()?;
    Some(linked(root, &marker).unwrap_or_else(|| Checkout {
        root: root.to_path_buf(),
        main_root: root.to_path_buf(),
        branch: branch(&marker),
        repository: RepositoryKey(marker),
        linked: None,
    }))
}

/// Every checkout git records for `repository`: the main checkout first
/// unless the repository is bare, then each linked worktree whose records
/// agree from both ends. One deleted without telling git is reported as
/// prunable, at the root git recorded for it.
pub fn members(repository: &RepositoryKey) -> Vec<CheckoutMember> {
    let common = repository.as_path();
    let main = (common.is_dir() && common.file_name() == Some(OsStr::new(GIT_MARKER))).then(|| {
        CheckoutMember {
            root: main_root(common).to_path_buf(),
            branch: branch(common),
            admin: None,
            prunable: false,
        }
    });
    let mut linked: Vec<_> = fs::read_dir(common.join(WORKTREES_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| linked_member(repository, &entry.path()))
        .collect();
    linked.sort_by(|left, right| left.root.cmp(&right.root));
    main.into_iter().chain(linked).collect()
}

/// Roots of the other verified checkouts of the repository `project` is in.
pub fn sibling_roots(project: &Path) -> Vec<PathBuf> {
    discover(project).map_or_else(Vec::new, |checkout| other_roots(&checkout))
}

/// `project` followed by its counterpart in each sibling checkout.
pub(crate) fn with_siblings(project: &Path) -> Vec<PathBuf> {
    iter::once(project.to_path_buf())
        .chain(sibling_projects(project))
        .collect()
}

/// `project`'s counterpart in each sibling checkout: the same path relative
/// to that checkout's root, where it exists and stays inside it.
fn sibling_projects(project: &Path) -> Vec<PathBuf> {
    let Ok(project) = project.canonicalize() else {
        return Vec::new();
    };
    let Some(checkout) = discover(&project) else {
        return Vec::new();
    };
    let Ok(relative) = project.strip_prefix(&checkout.root) else {
        return Vec::new();
    };
    other_roots(&checkout)
        .into_iter()
        .filter_map(|root| {
            root.join(relative)
                .canonicalize()
                .ok()
                .filter(|path| path.starts_with(&root))
        })
        .collect()
}

fn other_roots(checkout: &Checkout) -> Vec<PathBuf> {
    members(&checkout.repository)
        .into_iter()
        .filter(|member| !member.prunable && member.root != checkout.root)
        .map(|member| member.root)
        .collect()
}

/// The linked worktree at `root`, when git's records agree from both ends.
fn linked(root: &Path, marker: &Path) -> Option<Checkout> {
    let admin = recorded_path(marker, GITDIR_PREFIX)?;
    let common = admin
        .parent()
        .filter(|worktrees| worktrees.file_name() == Some(OsStr::new(WORKTREES_DIR)))?
        .parent()?;
    if recorded_path(&admin.join(COMMONDIR_FILE), "")? != common
        || recorded_path(&admin.join(GITDIR_FILE), "")? != marker
    {
        return None;
    }
    Some(Checkout {
        root: root.to_path_buf(),
        main_root: main_root(common).to_path_buf(),
        repository: RepositoryKey(common.to_path_buf()),
        linked: Some(admin.file_name()?.to_string_lossy().into_owned()),
        branch: branch(&admin),
    })
}

fn linked_member(repository: &RepositoryKey, admin: &Path) -> Option<CheckoutMember> {
    let name = admin.file_name()?.to_string_lossy().into_owned();
    let recorded = fs::read_to_string(admin.join(GITDIR_FILE)).ok()?;
    let marker = admin.join(recorded.trim_end());
    let root = marker.parent()?;
    if !marker.exists() {
        return Some(CheckoutMember {
            root: normalize_path(root),
            branch: branch(admin),
            admin: Some(name),
            prunable: true,
        });
    }
    discover(root)
        .filter(|checkout| {
            checkout.repository == *repository && checkout.linked.as_ref() == Some(&name)
        })
        .map(|checkout| CheckoutMember {
            root: checkout.root,
            branch: checkout.branch,
            admin: checkout.linked,
            prunable: false,
        })
}

/// A path git recorded in `file`, resolved as git resolves it: relative to
/// the directory holding the file.
fn recorded_path(file: &Path, prefix: &str) -> Option<PathBuf> {
    let content = fs::read_to_string(file).ok()?;
    let recorded =
        Some(content.strip_prefix(prefix)?.trim_end()).filter(|recorded| !recorded.is_empty())?;
    file.parent()?.join(recorded).canonicalize().ok()
}

/// The common directory's parent when it is a checkout's `.git`, otherwise the
/// bare repository itself.
fn main_root(common: &Path) -> &Path {
    match common.parent() {
        Some(parent) if common.file_name() == Some(OsStr::new(GIT_MARKER)) => parent,
        _ => common,
    }
}

/// `None` when `HEAD` is detached or unreadable.
fn branch(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join(HEAD_FILE)).ok()?;
    head.strip_prefix(BRANCH_REF_PREFIX)
        .map(|name| name.trim_end().to_owned())
}

/// Git layouts written by hand, so tests need no git binary.
#[cfg(test)]
pub(crate) mod fixture {
    use std::fs;
    use std::iter;
    use std::path::{Component, Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        BRANCH_REF_PREFIX, COMMONDIR_FILE, GITDIR_FILE, GITDIR_PREFIX, HEAD_FILE, WORKTREES_DIR,
    };
    use crate::StateDir;
    use crate::projects::GIT_MARKER;

    pub(crate) const MAIN_BRANCH: &str = "main";
    pub(crate) const ADMIN: &str = "login";
    pub(crate) const BRANCH: &str = "feature/login";
    const MAIN_DIR: &str = "repo";
    const FORGED_DIR: &str = "forged";
    const STATE_DIR: &str = "state";

    /// How a linked worktree's records name each other.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Links {
        Absolute,
        Relative,
    }

    /// Canonical, because git records canonical paths and a temp root can sit
    /// behind a symlink.
    pub(crate) fn canonical_tempdir() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        (temp, base)
    }

    /// Makes `root` a main checkout on [`MAIN_BRANCH`], returning its git dir.
    pub(crate) fn main_checkout(root: &Path) -> PathBuf {
        let common = root.join(GIT_MARKER);
        git_dir(&common, MAIN_BRANCH);
        common
    }

    pub(crate) fn git_dir(path: &Path, branch: &str) {
        fs::create_dir_all(path).unwrap();
        fs::write(
            path.join(HEAD_FILE),
            format!("{BRANCH_REF_PREFIX}{branch}\n"),
        )
        .unwrap();
    }

    /// Registers `root` as linked worktree `admin` of `common`, returning the
    /// admin directory.
    pub(crate) fn linked_worktree(
        common: &Path,
        admin: &str,
        root: &Path,
        branch: &str,
        links: Links,
    ) -> PathBuf {
        let admin_dir = common.join(WORKTREES_DIR).join(admin);
        git_dir(&admin_dir, branch);
        fs::create_dir_all(root).unwrap();
        let marker = root.join(GIT_MARKER);
        let (to_common, to_marker, to_admin) = match links {
            Links::Absolute => (common.to_path_buf(), marker.clone(), admin_dir.clone()),
            Links::Relative => (
                relative(&admin_dir, common),
                relative(&admin_dir, &marker),
                relative(root, &admin_dir),
            ),
        };
        fs::write(
            admin_dir.join(COMMONDIR_FILE),
            format!("{}\n", to_common.display()),
        )
        .unwrap();
        fs::write(
            admin_dir.join(GITDIR_FILE),
            format!("{}\n", to_marker.display()),
        )
        .unwrap();
        write_git_file(root, &to_admin);
        admin_dir
    }

    /// A main checkout with linked worktree [`ADMIN`] on [`BRANCH`], returning
    /// both roots.
    pub(crate) fn linked_pair(base: &Path) -> (PathBuf, PathBuf) {
        let main = base.join(MAIN_DIR);
        let worktree = base.join(ADMIN);
        linked_worktree(
            &main_checkout(&main),
            ADMIN,
            &worktree,
            BRANCH,
            Links::Absolute,
        );
        (main, worktree)
    }

    /// A [`linked_pair`] with a state directory beside it, returning the
    /// state directory, the main root, and the worktree root.
    pub(crate) fn linked_pair_with_state() -> (TempDir, StateDir, PathBuf, PathBuf) {
        let (temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        (
            temp,
            StateDir::from_path(base.join(STATE_DIR)),
            main,
            worktree,
        )
    }

    /// A directory whose `.git` file claims the admin directory of
    /// [`linked_pair`]'s worktree, which does not name it back.
    pub(crate) fn forged_worktree(base: &Path, main: &Path) -> PathBuf {
        let forged = base.join(FORGED_DIR);
        write_git_file(
            &forged,
            &main.join(GIT_MARKER).join(WORKTREES_DIR).join(ADMIN),
        );
        forged
    }

    pub(crate) fn write_git_file(root: &Path, git_dir: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join(GIT_MARKER),
            format!("{GITDIR_PREFIX}{}\n", git_dir.display()),
        )
        .unwrap();
    }

    fn relative(from: &Path, to: &Path) -> PathBuf {
        let from: Vec<_> = from.components().collect();
        let to: Vec<_> = to.components().collect();
        let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
        iter::repeat_n(Component::ParentDir, from.len() - shared)
            .chain(to[shared..].iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::fixture::{
        ADMIN, BRANCH, Links, MAIN_BRANCH, canonical_tempdir, forged_worktree, git_dir,
        linked_pair, linked_worktree, main_checkout, write_git_file,
    };
    use super::*;

    const OTHER_ADMIN: &str = "gone";
    const OTHER_BRANCH: &str = "feature/gone";
    const NESTED: &str = "crates/app";
    const NOT_LINKED: &str = "an unverifiable .git file must not join a repository";

    #[test]
    fn a_git_directory_is_a_main_checkout_from_any_subdirectory() {
        let (_temp, base) = canonical_tempdir();
        let root = base.join("repo");
        let common = main_checkout(&root);
        let nested = root.join(NESTED);
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            discover(&nested),
            Some(Checkout {
                root: root.clone(),
                repository: RepositoryKey(common),
                main_root: root,
                linked: None,
                branch: Some(MAIN_BRANCH.into()),
            })
        );
    }

    #[test]
    fn outside_any_checkout_there_is_none() {
        let (_temp, base) = canonical_tempdir();

        assert_eq!(discover(&base), None);
    }

    #[test_case(Links::Absolute ; "absolute_links")]
    #[test_case(Links::Relative ; "relative_links")]
    fn a_linked_worktree_verifies_from_both_ends(links: Links) {
        let (_temp, base) = canonical_tempdir();
        let main = base.join("repo");
        let common = main_checkout(&main);
        let root = base.join(ADMIN);
        linked_worktree(&common, ADMIN, &root, BRANCH, links);
        let nested = root.join(NESTED);
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            discover(&nested),
            Some(Checkout {
                root,
                repository: RepositoryKey(common),
                main_root: main,
                linked: Some(ADMIN.into()),
                branch: Some(BRANCH.into()),
            })
        );
    }

    #[test]
    fn a_bare_repository_is_its_own_main_root() {
        let (_temp, base) = canonical_tempdir();
        let common = base.join("app.git");
        git_dir(&common, MAIN_BRANCH);
        let root = base.join(ADMIN);
        linked_worktree(&common, ADMIN, &root, BRANCH, Links::Absolute);

        let checkout = discover(&root).unwrap();

        assert!(checkout.is_linked() && checkout.is_bare());
        assert_eq!(checkout.main_root, common);
        assert_eq!(
            members(&checkout.repository)
                .into_iter()
                .map(|member| member.root)
                .collect::<Vec<_>>(),
            vec![root]
        );
    }

    fn forged_back_link_layout(base: &Path) -> PathBuf {
        let (main, _) = linked_pair(base);
        forged_worktree(base, &main)
    }

    /// A submodule's git dir has no `commondir`, so it is no linked worktree.
    fn submodule_layout(base: &Path) -> PathBuf {
        let common = main_checkout(&base.join("super"));
        let modules = common.join("modules/sub");
        git_dir(&modules, MAIN_BRANCH);
        let sub = base.join("super/sub");
        write_git_file(&sub, &modules);
        sub
    }

    /// A `commondir` naming another repository than the one holding the admin
    /// directory.
    fn foreign_commondir_layout(base: &Path) -> PathBuf {
        let common = main_checkout(&base.join("repo"));
        let other = main_checkout(&base.join("other"));
        let root = base.join(ADMIN);
        let admin = linked_worktree(&common, ADMIN, &root, BRANCH, Links::Absolute);
        fs::write(
            admin.join(COMMONDIR_FILE),
            other.to_string_lossy().as_bytes(),
        )
        .unwrap();
        root
    }

    #[test_case(forged_back_link_layout ; "forged_back_link")]
    #[test_case(submodule_layout ; "submodule")]
    #[test_case(foreign_commondir_layout ; "foreign_commondir")]
    fn an_unverifiable_git_file_is_a_checkout_of_its_own(layout: fn(&Path) -> PathBuf) {
        let (_temp, base) = canonical_tempdir();
        let root = layout(&base);

        let checkout = discover(&root).unwrap();

        assert_eq!(checkout.linked, None, "{NOT_LINKED}");
        assert_eq!(checkout.root, root);
        assert_eq!(checkout.main_root, root);
        assert_eq!(checkout.repository.as_path(), root.join(GIT_MARKER));
        assert!(sibling_roots(&root).is_empty(), "{NOT_LINKED}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_resolves_to_the_canonical_checkout() {
        let (_temp, base) = canonical_tempdir();
        let (_, worktree) = linked_pair(&base);
        let link = base.join("link");
        std::os::unix::fs::symlink(&worktree, &link).unwrap();

        let checkout = discover(&link).unwrap();

        assert_eq!(checkout.root, worktree);
        assert_eq!(checkout.linked.as_deref(), Some(ADMIN));
    }

    #[test]
    fn members_list_the_main_checkout_first_and_flag_removed_worktrees() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let common = main.join(GIT_MARKER);
        let removed = base.join(OTHER_ADMIN);
        linked_worktree(
            &common,
            OTHER_ADMIN,
            &removed,
            OTHER_BRANCH,
            Links::Relative,
        );
        fs::remove_dir_all(&removed).unwrap();

        assert_eq!(
            members(&RepositoryKey(common)),
            vec![
                CheckoutMember {
                    root: main,
                    branch: Some(MAIN_BRANCH.into()),
                    admin: None,
                    prunable: false,
                },
                CheckoutMember {
                    root: removed,
                    branch: Some(OTHER_BRANCH.into()),
                    admin: Some(OTHER_ADMIN.into()),
                    prunable: true,
                },
                CheckoutMember {
                    root: worktree,
                    branch: Some(BRANCH.into()),
                    admin: Some(ADMIN.into()),
                    prunable: false,
                },
            ]
        );
    }

    #[test]
    fn sibling_roots_exclude_the_checkout_itself_and_removed_worktrees() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let removed = base.join(OTHER_ADMIN);
        linked_worktree(
            &main.join(GIT_MARKER),
            OTHER_ADMIN,
            &removed,
            OTHER_BRANCH,
            Links::Absolute,
        );
        fs::remove_dir_all(&removed).unwrap();
        let nested = worktree.join(NESTED);
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(sibling_roots(&main), vec![worktree]);
        assert_eq!(sibling_roots(&nested), vec![main]);
    }

    #[test]
    fn a_subdirectory_maps_to_the_same_subdirectory_of_each_sibling() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let only_here = worktree.join("crates/new");
        fs::create_dir_all(main.join(NESTED)).unwrap();
        fs::create_dir_all(worktree.join(NESTED)).unwrap();
        fs::create_dir_all(&only_here).unwrap();

        assert_eq!(
            with_siblings(&worktree.join(NESTED)),
            vec![worktree.join(NESTED), main.join(NESTED)]
        );
        assert_eq!(with_siblings(&only_here), vec![only_here]);
    }
}
