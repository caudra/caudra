//! The git commands behind `/worktree`: run directly rather than through a
//! shell, never prompting, and bounded in time. Nothing here deletes a branch
//! or touches `safe.directory`.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use thiserror::Error;

use crate::bounded_process::{self, Finished, ProcessError};

const GIT: &str = "git";
const TERMINAL_PROMPT_VAR: &str = "GIT_TERMINAL_PROMPT";
const PROMPT_DISABLED: &str = "0";
/// A query reads a little of the repository.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Adding, removing or stashing writes or reads a whole tree, and runs hooks.
const TREE_TIMEOUT: Duration = Duration::from_secs(600);
const STASH_REF: &str = "refs/stash";
const BRANCH_REF_PREFIX: &str = "refs/heads/";
/// How `show-ref --verify --quiet` and `rev-parse --quiet --verify` say a ref
/// does not exist.
const MISSING_EXIT_CODE: i32 = 1;
const OPTION_PREFIX: char = '-';

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git {command}: {source}")]
    Run {
        command: &'static str,
        #[source]
        source: ProcessError,
    },
    #[error("git {command} failed: {stderr}")]
    Failed {
        command: &'static str,
        stderr: String,
    },
    #[error("git {command} printed output that is not UTF-8")]
    Output { command: &'static str },
    #[error("a branch name cannot start with '-': {0}")]
    OptionLikeBranch(String),
    #[error("stash {0} is no longer in the stash list")]
    StashMissing(String),
}

/// Git, run in one directory.
#[derive(Debug, Clone, Copy)]
pub struct Git<'a> {
    dir: &'a Path,
}

impl<'a> Git<'a> {
    pub fn new(dir: &'a Path) -> Self {
        Self { dir }
    }

    /// Whether the checkout has changes another checkout would not see:
    /// staged, unstaged or untracked.
    pub fn is_dirty(&self) -> Result<bool, GitError> {
        let status = self.output(
            "status",
            &["--no-optional-locks", "status", "--porcelain"],
            QUERY_TIMEOUT,
        )?;
        Ok(!status.is_empty())
    }

    pub fn head(&self) -> Result<String, GitError> {
        self.commit("HEAD")
    }

    /// The commit `revision` names.
    pub fn commit(&self, revision: &str) -> Result<String, GitError> {
        self.output(
            "rev-parse",
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ],
            QUERY_TIMEOUT,
        )
    }

    /// `name` as git stores it, refused when it cannot name a branch.
    pub fn branch_name(&self, name: &str) -> Result<String, GitError> {
        if name.starts_with(OPTION_PREFIX) {
            return Err(GitError::OptionLikeBranch(name.to_owned()));
        }
        self.output(
            "check-ref-format",
            &["check-ref-format", "--branch", name],
            QUERY_TIMEOUT,
        )
    }

    pub fn has_branch(&self, name: &str) -> Result<bool, GitError> {
        let reference = format!("{BRANCH_REF_PREFIX}{name}");
        Ok(self
            .probe("show-ref", &["show-ref", "--verify", "--quiet", &reference])?
            .is_some())
    }

    /// Adds a checkout at `path` on `branch`, starting the branch at `base`
    /// when it does not exist yet.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        let mut args: Vec<&OsStr> = vec!["worktree".as_ref(), "add".as_ref()];
        if self.has_branch(branch)? {
            args.extend([path.as_os_str(), branch.as_ref()]);
        } else {
            args.extend([
                "-b".as_ref(),
                branch.as_ref(),
                path.as_os_str(),
                base.as_ref(),
            ]);
        }
        self.output("worktree add", &args, TREE_TIMEOUT).map(drop)
    }

    /// Removes the checkout at `path`, keeping its branch. `force` discards
    /// its uncommitted changes.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), GitError> {
        let mut args: Vec<&OsStr> = vec!["worktree".as_ref(), "remove".as_ref()];
        if force {
            args.push("--force".as_ref());
        }
        args.push(path.as_os_str());
        self.output("worktree remove", &args, TREE_TIMEOUT)
            .map(drop)
    }

    /// Stashes every change, untracked files included, and returns the stash
    /// commit, or `None` when there was nothing to stash. Every checkout of
    /// the repository shares the stash.
    pub fn stash_push(&self, message: &str) -> Result<Option<String>, GitError> {
        let before = self.stash_top()?;
        self.output(
            "stash push",
            &["stash", "push", "--include-untracked", "--message", message],
            TREE_TIMEOUT,
        )?;
        Ok(self
            .stash_top()?
            .filter(|after| before.as_ref() != Some(after)))
    }

    pub fn stash_apply(&self, stash: &str) -> Result<(), GitError> {
        self.output("stash apply", &["stash", "apply", stash], TREE_TIMEOUT)
            .map(drop)
    }

    /// Drops `stash` from the stash list, wherever it sits in it by now.
    pub fn stash_drop(&self, stash: &str) -> Result<(), GitError> {
        let list = self.output(
            "stash list",
            &["stash", "list", "--format=%H"],
            QUERY_TIMEOUT,
        )?;
        let index = list
            .lines()
            .position(|line| line == stash)
            .ok_or_else(|| GitError::StashMissing(stash.to_owned()))?;
        self.output(
            "stash drop",
            &["stash", "drop", &format!("stash@{{{index}}}")],
            QUERY_TIMEOUT,
        )
        .map(drop)
    }

    fn stash_top(&self) -> Result<Option<String>, GitError> {
        self.probe(
            "rev-parse",
            &["rev-parse", "--quiet", "--verify", STASH_REF],
        )
    }

    /// Stdout of a command that has to succeed, without its final newline.
    fn output<S: AsRef<OsStr>>(
        &self,
        command: &'static str,
        args: &[S],
        timeout: Duration,
    ) -> Result<String, GitError> {
        let finished = self.run(command, args, timeout)?;
        if !finished.status.success() {
            return Err(failure(command, &finished));
        }
        text(command, finished.stdout)
    }

    /// Stdout of a check that exits 1 to say no.
    fn probe<S: AsRef<OsStr>>(
        &self,
        command: &'static str,
        args: &[S],
    ) -> Result<Option<String>, GitError> {
        let finished = self.run(command, args, QUERY_TIMEOUT)?;
        match finished.status.code() {
            Some(0) => text(command, finished.stdout).map(Some),
            Some(MISSING_EXIT_CODE) => Ok(None),
            _ => Err(failure(command, &finished)),
        }
    }

    fn run<S: AsRef<OsStr>>(
        &self,
        command: &'static str,
        args: &[S],
        timeout: Duration,
    ) -> Result<Finished, GitError> {
        let mut git = Command::new(GIT);
        git.arg("-C")
            .arg(self.dir)
            .args(args)
            .env(TERMINAL_PROMPT_VAR, PROMPT_DISABLED);
        bounded_process::run(&mut git, timeout).map_err(|source| GitError::Run { command, source })
    }
}

fn failure(command: &'static str, finished: &Finished) -> GitError {
    GitError::Failed {
        command,
        stderr: String::from_utf8_lossy(finished.stderr.trim_ascii()).into_owned(),
    }
}

fn text(command: &'static str, stdout: Vec<u8>) -> Result<String, GitError> {
    String::from_utf8(stdout)
        .map(|text| text.trim_end().to_owned())
        .map_err(|_| GitError::Output { command })
}

/// A repository with one commit, in a temporary directory.
#[cfg(test)]
pub(crate) mod fixture {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    use super::{GIT, Git};

    pub(crate) const TRACKED: &str = "tracked.txt";
    pub(crate) const UNTRACKED: &str = "notes.txt";
    pub(crate) const ORIGINAL: &str = "original\n";
    pub(crate) const EDITED: &str = "edited\n";

    pub(crate) struct Repository {
        temp: TempDir,
        pub(crate) root: PathBuf,
    }

    impl Repository {
        pub(crate) fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = temp.path().canonicalize().unwrap().join("main");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "--quiet", "--initial-branch=main"]);
            git(&root, &["config", "user.email", "caudra@example.com"]);
            git(&root, &["config", "user.name", "Caudra"]);
            git(&root, &["config", "commit.gpgsign", "false"]);
            fs::write(root.join(TRACKED), ORIGINAL).unwrap();
            git(&root, &["add", TRACKED]);
            git(&root, &["commit", "--quiet", "--message", "initial"]);
            Self { temp, root }
        }

        pub(crate) fn git(&self) -> Git<'_> {
            Git::new(&self.root)
        }

        /// A path beside the repository, inside the same temporary directory.
        pub(crate) fn sibling(&self, name: &str) -> PathBuf {
            self.temp.path().canonicalize().unwrap().join(name)
        }

        pub(crate) fn edit(&self, root: &Path) {
            fs::write(root.join(TRACKED), EDITED).unwrap();
            fs::write(root.join(UNTRACKED), EDITED).unwrap();
        }
    }

    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let status = Command::new(GIT)
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    pub(crate) fn stashes(dir: &Path) -> usize {
        let output = Command::new(GIT)
            .arg("-C")
            .arg(dir)
            .args(["stash", "list"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().lines().count()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::fixture::{EDITED, Repository, TRACKED, UNTRACKED, git};
    use super::{Git, GitError};

    const BRANCH: &str = "feature/login";
    const STASH_MESSAGE: &str = "caudra: carry";

    #[test]
    fn a_fresh_commit_is_clean_and_an_edit_is_dirty() {
        let repository = Repository::new();
        assert!(!repository.git().is_dirty().unwrap());

        fs::write(repository.root.join(UNTRACKED), EDITED).unwrap();

        assert!(repository.git().is_dirty().unwrap());
    }

    #[test_case(BRANCH, true ; "nested_name")]
    #[test_case("-delete", false ; "option_like_name")]
    #[test_case("two words", false ; "space")]
    #[test_case("ends.lock", false ; "lock_suffix")]
    fn branch_names_are_checked_by_git(name: &str, valid: bool) {
        let repository = Repository::new();

        assert_eq!(repository.git().branch_name(name).is_ok(), valid);
    }

    #[test]
    fn a_worktree_starts_a_new_branch_at_the_base_and_keeps_it_when_removed() {
        let repository = Repository::new();
        let head = repository.git().head().unwrap();
        let path = repository.sibling("login");

        repository.git().add_worktree(&path, BRANCH, &head).unwrap();

        assert_eq!(Git::new(&path).head().unwrap(), head);
        repository.git().remove_worktree(&path, false).unwrap();
        assert!(!path.exists());
        assert!(repository.git().has_branch(BRANCH).unwrap());
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        let repository = Repository::new();
        let head = repository.git().head().unwrap();
        git(&repository.root, &["branch", BRANCH]);
        git(
            &repository.root,
            &["commit", "--quiet", "--allow-empty", "--message", "later"],
        );
        let path = repository.sibling("login");

        repository
            .git()
            .add_worktree(&path, BRANCH, &repository.git().head().unwrap())
            .unwrap();

        assert_eq!(Git::new(&path).head().unwrap(), head);
    }

    #[test]
    fn a_dirty_worktree_is_removed_only_by_force() {
        let repository = Repository::new();
        let head = repository.git().head().unwrap();
        let path = repository.sibling("login");
        repository.git().add_worktree(&path, BRANCH, &head).unwrap();
        fs::write(path.join(UNTRACKED), EDITED).unwrap();

        assert!(matches!(
            repository.git().remove_worktree(&path, false),
            Err(GitError::Failed { .. })
        ));
        repository.git().remove_worktree(&path, true).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn stashed_changes_carry_into_another_checkout_and_leave_the_list() {
        let repository = Repository::new();
        let head = repository.git().head().unwrap();
        let path = repository.sibling("login");
        repository.git().add_worktree(&path, BRANCH, &head).unwrap();
        repository.edit(&repository.root);

        let stash = repository.git().stash_push(STASH_MESSAGE).unwrap().unwrap();
        Git::new(&path).stash_apply(&stash).unwrap();
        repository.git().stash_drop(&stash).unwrap();

        assert!(!repository.git().is_dirty().unwrap());
        assert_eq!(fs::read_to_string(path.join(TRACKED)).unwrap(), EDITED);
        assert_eq!(fs::read_to_string(path.join(UNTRACKED)).unwrap(), EDITED);
        assert!(matches!(
            repository.git().stash_drop(&stash),
            Err(GitError::StashMissing(_))
        ));
    }

    #[test]
    fn a_clean_checkout_has_nothing_to_stash() {
        let repository = Repository::new();

        assert_eq!(repository.git().stash_push(STASH_MESSAGE).unwrap(), None);
    }
}
