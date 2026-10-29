use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use caudra_automation::host::{Failure, FailureKind, GitCompare, GitOperation, MAX_GIT_COMMITS};
use caudra_automation::untrusted::Untrusted;
use gix::bstr::ByteSlice;
use gix::refs::FullName;
use gix::state::InProgress;
use gix::{ObjectId, Repository};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const THREAD_NAME: &str = "automation-git";
const BRANCH_PREFIX: &str = "refs/heads/";
const INVALID_BASE: &str = "git_compare needs a local branch name";
const TIMED_OUT: &str = "git_compare exceeded its five-second deadline";
const WORKER_GONE: &str = "git_compare worker stopped without an answer";

pub(super) fn compare(cwd: PathBuf, base: &str) -> Result<Option<GitCompare>, Failure> {
    let reference = branch_ref(base)?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let _ = send.send(compare_at(&cwd, &reference, deadline));
        })
        .map_err(unavailable)?;
    receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            RecvTimeoutError::Timeout => timeout(),
            RecvTimeoutError::Disconnected => unavailable(WORKER_GONE),
        })?
}

fn branch_ref(base: &str) -> Result<FullName, Failure> {
    if base.is_empty() || base.starts_with('-') || base == "HEAD" || base == "@" {
        return Err(Failure::new(FailureKind::InvalidArgument, INVALID_BASE));
    }
    FullName::try_from(format!("{BRANCH_PREFIX}{base}"))
        .map_err(|_| Failure::new(FailureKind::InvalidArgument, INVALID_BASE))
}

fn compare_at(
    cwd: &Path,
    base: &FullName,
    deadline: Instant,
) -> Result<Option<GitCompare>, Failure> {
    let repo = match gix::discover(cwd) {
        Ok(repo) => repo,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    let mut reference = match repo.find_reference(base) {
        Ok(reference) => reference,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    let base_head = reference.peel_to_id().map_err(unavailable)?.detach();
    let head = repo.head_id().map_err(unavailable)?.detach();
    let branch = repo
        .head_name()
        .map_err(unavailable)?
        .map(|name| Untrusted::text(name.shorten().to_str_lossy()));
    let operation = repo.state().map(|operation| match operation {
        InProgress::Merge => GitOperation::Merge,
        InProgress::Rebase
        | InProgress::RebaseInteractive
        | InProgress::ApplyMailbox
        | InProgress::ApplyMailboxRebase => GitOperation::Rebase,
        InProgress::CherryPick | InProgress::CherryPickSequence => GitOperation::CherryPick,
        InProgress::Revert | InProgress::RevertSequence => GitOperation::Revert,
        InProgress::Bisect => GitOperation::Bisect,
    });
    let (ahead, behind) = if head == base_head {
        (0, 0)
    } else {
        (
            count(&repo, head, base_head, deadline)?,
            count(&repo, base_head, head, deadline)?,
        )
    };
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    Ok(Some(GitCompare {
        branch,
        head: head.to_string(),
        base_head: base_head.to_string(),
        ahead,
        behind,
        operation,
    }))
}

fn count(
    repo: &Repository,
    tip: ObjectId,
    hidden: ObjectId,
    deadline: Instant,
) -> Result<u32, Failure> {
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .selected(move |_| Instant::now() < deadline)
        .map_err(unavailable)?;
    let mut count = 0;
    for commit in walk.take(MAX_GIT_COMMITS as usize) {
        commit.map_err(unavailable)?;
        count += 1;
    }
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    Ok(count)
}

fn unavailable(error: impl Display) -> Failure {
    Failure::new(
        FailureKind::Unavailable,
        format!("git_compare could not read the repository: {error}"),
    )
}

fn timeout() -> Failure {
    Failure::new(FailureKind::Timeout, TIMED_OUT)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::process::Command;

    use gix::actor::SignatureRef;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        FailureKind, GitOperation, INVALID_BASE, Instant, MAX_GIT_COMMITS, ObjectId, Path,
        QUERY_TIMEOUT, Repository, TIMED_OUT, Untrusted, branch_ref, compare, compare_at, count,
    };

    const BASE: &str = "main";
    const BASE_REF: &str = "refs/heads/main";
    const BRANCH: &str = "feature";
    const BRANCH_REF: &str = "refs/heads/feature";
    const HEAD_REF: &str = "ref: refs/heads/feature\n";
    const AUTHOR: &str = "Automation test";
    const EMAIL: &str = "automation@example.com";
    const TIME: &str = "1700000000 +0000";
    const INITIAL: &str = "initial";
    const LINKED: &str = "linked";
    const MISSING: &str = "missing";
    const BENCH_REPO: &str = "CAUDRA_BENCH_REPO";
    const BENCH_RUNS: usize = 50;
    const BENCH_RANGE: &str = "HEAD~50..HEAD";
    const BENCH_ANCESTOR: &str = "HEAD~50";
    const PERCENTILE: usize = 95;
    const PERCENT: usize = 100;

    fn commit(repo: &Repository, reference: &str, message: &str, parents: &[ObjectId]) -> ObjectId {
        let signature = SignatureRef {
            name: AUTHOR.into(),
            email: EMAIL.into(),
            time: TIME,
        };
        let tree = repo
            .write_object(gix::objs::Tree::empty())
            .unwrap()
            .detach();
        repo.commit_as(
            signature,
            signature,
            reference,
            message,
            tree,
            parents.iter().copied(),
        )
        .unwrap()
        .detach()
    }

    fn fixture(ahead: u32, behind: u32) -> (TempDir, ObjectId, ObjectId) {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let root = commit(&repo, BASE_REF, INITIAL, &[]);
        fs::write(repo.path().join(BRANCH_REF), root.to_string()).unwrap();
        fs::write(repo.path().join("HEAD"), HEAD_REF).unwrap();
        let mut base = root;
        let mut head = root;
        for index in 0..behind {
            base = commit(&repo, BASE_REF, &format!("{BASE}-{index}"), &[base]);
        }
        for index in 0..ahead {
            head = commit(&repo, BRANCH_REF, &format!("{BRANCH}-{index}"), &[head]);
        }
        (dir, head, base)
    }

    #[test_case(0, 2; "behind")]
    #[test_case(2, 0; "ahead")]
    #[test_case(2, 3; "diverged")]
    #[test_case(0, 0; "current")]
    fn compares_reachable_commits(ahead: u32, behind: u32) {
        let (dir, head, base) = fixture(ahead, behind);
        let comparison = compare(dir.path().to_owned(), BASE).unwrap().unwrap();
        assert_eq!((comparison.ahead, comparison.behind), (ahead, behind));
        assert_eq!(comparison.branch, Some(Untrusted::text(BRANCH)));
        assert_eq!(
            (comparison.head, comparison.base_head),
            (head.to_string(), base.to_string())
        );
        assert_eq!(comparison.operation, None);
    }

    #[test_case(false; "missing_base")]
    #[test_case(true; "outside_repository")]
    fn missing_comparisons_return_none(outside: bool) {
        let dir = if outside {
            tempfile::tempdir().unwrap()
        } else {
            fixture(0, 0).0
        };
        assert_eq!(compare(dir.path().to_owned(), MISSING).unwrap(), None);
    }

    #[test_case(false; "detached_head")]
    #[test_case(true; "linked_worktree")]
    fn reads_worktree_head(linked: bool) {
        let (dir, head, _) = fixture(1, 2);
        let cwd = if linked {
            let cwd = dir.path().join(LINKED);
            let git_dir = dir.path().join(".git/worktrees").join(LINKED);
            fs::create_dir_all(&git_dir).unwrap();
            fs::create_dir_all(&cwd).unwrap();
            fs::write(cwd.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
            fs::write(git_dir.join("commondir"), "../..\n").unwrap();
            fs::write(git_dir.join("gitdir"), cwd.join(".git").to_str().unwrap()).unwrap();
            fs::write(git_dir.join("HEAD"), HEAD_REF).unwrap();
            fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            cwd
        } else {
            fs::write(dir.path().join(".git/HEAD"), head.to_string()).unwrap();
            dir.path().to_owned()
        };
        let comparison = compare(cwd, BASE).unwrap().unwrap();
        assert_eq!(comparison.branch, linked.then(|| Untrusted::text(BRANCH)));
        assert_eq!((comparison.ahead, comparison.behind), (1, 2));
    }

    #[test_case("MERGE_HEAD", GitOperation::Merge; "merge")]
    #[test_case("rebase-merge/interactive", GitOperation::Rebase; "rebase")]
    #[test_case("CHERRY_PICK_HEAD", GitOperation::CherryPick; "cherry_pick")]
    #[test_case("REVERT_HEAD", GitOperation::Revert; "revert")]
    #[test_case("BISECT_LOG", GitOperation::Bisect; "bisect")]
    fn reports_in_progress_operation(marker: &str, operation: GitOperation) {
        let (dir, _, base) = fixture(1, 1);
        let path = dir.path().join(".git").join(marker);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, base.to_string()).unwrap();
        assert_eq!(
            compare(dir.path().to_owned(), BASE)
                .unwrap()
                .unwrap()
                .operation,
            Some(operation)
        );
    }

    #[test_case("")]
    #[test_case("../main")]
    #[test_case("main~1")]
    #[test_case("-main")]
    #[test_case("main.lock")]
    #[test_case("main\nother")]
    #[test_case("HEAD")]
    fn rejects_invalid_branch_names(base: &str) {
        let error = compare(Path::new(".").to_owned(), base).unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (FailureKind::InvalidArgument, INVALID_BASE)
        );
    }

    #[test]
    fn traversal_observes_deadline() {
        let (dir, _, _) = fixture(1, 1);
        let error = compare_at(dir.path(), &branch_ref(BASE).unwrap(), Instant::now()).unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (FailureKind::Timeout, TIMED_OUT)
        );
    }

    #[test]
    fn commit_count_is_capped() {
        let (dir, _, base) = fixture(MAX_GIT_COMMITS + 1, 0);
        let repo = gix::open(dir.path()).unwrap();
        assert_eq!(
            count(
                &repo,
                repo.head_id().unwrap().detach(),
                base,
                Instant::now() + QUERY_TIMEOUT
            )
            .unwrap(),
            MAX_GIT_COMMITS
        );
    }

    #[test]
    #[ignore = "set CAUDRA_BENCH_REPO to a repository with at least 50 commits"]
    fn git_compare_benchmark() {
        let cwd = env::var_os(BENCH_REPO).expect(BENCH_REPO);
        let cwd = Path::new(&cwd);
        let repo = gix::discover(cwd).unwrap();
        let head = repo.head_id().unwrap().detach();
        let ancestor = repo.rev_parse_single(BENCH_ANCESTOR).unwrap().detach();
        let lagging = tempfile::tempdir().unwrap();
        let lagging_repo = gix::init(lagging.path()).unwrap();
        fs::create_dir_all(lagging_repo.path().join("objects/info")).unwrap();
        fs::write(
            lagging_repo.path().join("objects/info/alternates"),
            repo.common_dir()
                .join("objects")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        fs::write(lagging_repo.path().join(BASE_REF), head.to_string()).unwrap();
        fs::write(lagging_repo.path().join(BRANCH_REF), ancestor.to_string()).unwrap();
        fs::write(lagging_repo.path().join("HEAD"), HEAD_REF).unwrap();
        let mut floors = Vec::new();
        let mut probes = Vec::new();
        let mut behind_probes = Vec::new();
        for _ in 0..BENCH_RUNS {
            let started = Instant::now();
            let output = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["rev-list", "--count", BENCH_RANGE])
                .output()
                .unwrap();
            floors.push(started.elapsed());
            assert!(output.status.success());
            let expected: u32 = String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let started = Instant::now();
            let comparison = compare(cwd.to_owned(), BASE).unwrap().unwrap();
            probes.push(started.elapsed());
            assert_eq!(comparison.head, head.to_string());
            let started = Instant::now();
            let comparison = compare(lagging.path().to_owned(), BASE).unwrap().unwrap();
            behind_probes.push(started.elapsed());
            assert_eq!(comparison.head, ancestor.to_string());
            assert_eq!(comparison.base_head, head.to_string());
            assert_eq!(
                (comparison.ahead, comparison.behind),
                (0, expected.min(MAX_GIT_COMMITS))
            );
        }
        for (name, mut samples) in [
            ("git floor", floors),
            ("git_compare current repository", probes),
            ("git_compare HEAD~50 behind", behind_probes),
        ] {
            samples.sort();
            eprintln!(
                "{name}: n={BENCH_RUNS} p50={:?} p95={:?} range={:?}..{:?}",
                samples[BENCH_RUNS / 2],
                samples[(BENCH_RUNS * PERCENTILE).div_ceil(PERCENT) - 1],
                samples[0],
                samples[BENCH_RUNS - 1]
            );
        }
    }
}
