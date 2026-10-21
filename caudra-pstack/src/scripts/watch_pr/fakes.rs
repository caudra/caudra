//! Port of upstream's `fakes.test-helper.ts`: a scripted [`GitHubReader`] that records each call,
//! and the check fixtures the tests share.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;

use super::types::{
    Check, CheckKind, ChecksFastPath, CommitRollup, GitHubReader, MergeStateStatus, MergeableState,
    OpenPullRequest, PrContext, PrNumber, PrState, PullRequestFacts, RawReviewThread, Repository,
    ReviewDecision, ReviewThread, ReviewThreadPage, RollupPage, RollupState, WatchError,
};
use crate::host::fake::FakeHost;
use crate::host::{Cancelled, Host, Output};

pub const OWNER: &str = "owner";
pub const REPO: &str = "repo";
pub const CI: &str = "ci";
pub const HEAD: &str = "head";
pub const SLEEP: &str = "sleep";
pub const CWD: &str = "/work";

pub fn fake_host() -> FakeHost {
    FakeHost::new(CWD)
}

pub fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| (*arg).to_owned()).collect()
}

/// One ordered record of reads, emits, and sleeps, shared by a test's reader, emitter, and host.
pub type Timeline = Arc<Mutex<Vec<String>>>;

pub fn push(timeline: &Timeline, entry: impl Into<String>) {
    timeline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(entry.into());
}

pub fn entries(timeline: &Timeline) -> Vec<String> {
    timeline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// A [`FakeHost`] that also writes each sleep into a [`Timeline`], so a test sees it in order
/// with the reads and emits around it.
pub struct TimelineHost {
    pub inner: FakeHost,
    pub timeline: Timeline,
}

impl Host for TimelineHost {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.env_var(name)
    }

    fn run(&self, argv: &[&str], cwd: Option<&Path>) -> io::Result<Output> {
        self.inner.run(argv, cwd)
    }

    fn now(&self) -> Timestamp {
        self.inner.now()
    }

    fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
        push(&self.timeline, SLEEP);
        self.inner.sleep(duration)
    }

    fn progress(&self, line: &str) {
        self.inner.progress(line);
    }
}

type PullRequestHook =
    Box<dyn Fn(&PrContext, PullRequestFacts) -> Result<PullRequestFacts, WatchError>>;

pub fn context(number: PrNumber) -> PrContext {
    PrContext {
        owner: OWNER.to_owned(),
        repo: REPO.to_owned(),
        number,
    }
}

pub fn default_facts() -> PullRequestFacts {
    PullRequestFacts {
        context: context(1),
        mergeable: MergeableState::Mergeable,
        merge_state_status: MergeStateStatus::Clean,
        review_decision: Some(ReviewDecision::Approved),
        head_ref_oid: Some(HEAD.to_owned()),
        head_ref_name: "feature".to_owned(),
        base_ref_name: "main".to_owned(),
        state: PrState::Open,
        merged_at: None,
        is_draft: false,
    }
}

fn check(name: &str, kind: CheckKind, reported_state: &str) -> Check {
    Check {
        name: name.to_owned(),
        description: String::new(),
        link: String::new(),
        workflow: String::new(),
        kind,
        reported_state: reported_state.to_owned(),
    }
}

pub fn passing_check(name: &str) -> Check {
    check(name, CheckKind::Passed, "SUCCESS")
}

pub fn pending_check(name: &str) -> Check {
    check(name, CheckKind::Pending, "PENDING")
}

pub fn failed_check(name: &str) -> Check {
    check(name, CheckKind::Failed, "FAILURE")
}

pub struct FakeReader {
    pub facts: PullRequestFacts,
    pub fast_path: ChecksFastPath,
    pub rollup_pages: Vec<RollupPage>,
    pub threads: Vec<ReviewThread>,
    pub thread_pages: Vec<ReviewThreadPage>,
    pub commit_rollups: Vec<CommitRollup>,
    pub open_pull_requests: Vec<OpenPullRequest>,
    pub origin: Option<Repository>,
    pub current: PrContext,
    /// Rewrites, or fails, what `pull_request` answers, like upstream's wrapping readers.
    pub on_pull_request: Option<PullRequestHook>,
    pub calls: RefCell<Vec<String>>,
    pub page: Cell<usize>,
    pub thread_page: Cell<usize>,
}

impl Default for FakeReader {
    fn default() -> Self {
        Self {
            facts: default_facts(),
            fast_path: ChecksFastPath::Checks(vec![passing_check(CI)]),
            rollup_pages: Vec::new(),
            threads: Vec::new(),
            thread_pages: Vec::new(),
            commit_rollups: vec![CommitRollup {
                oid: HEAD.to_owned(),
                state: Some(RollupState::Success),
            }],
            open_pull_requests: Vec::new(),
            origin: Some(Repository {
                owner: OWNER.to_owned(),
                repo: REPO.to_owned(),
            }),
            current: context(1),
            on_pull_request: None,
            calls: RefCell::new(Vec::new()),
            page: Cell::new(0),
            thread_page: Cell::new(0),
        }
    }
}

impl FakeReader {
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.borrow_mut().push(call.into());
    }

    fn next<T: Clone>(pages: &[T], index: &Cell<usize>) -> Option<T> {
        let page = pages.get(index.get()).cloned();
        index.set(index.get() + 1);
        page
    }
}

fn cursor(after: Option<&str>) -> &str {
    after.unwrap_or("null")
}

impl GitHubReader for FakeReader {
    fn origin_repo(&self) -> Result<Option<Repository>, WatchError> {
        self.record("originRepo");
        Ok(self.origin.clone())
    }

    fn current_pr(&self, pr: Option<PrNumber>) -> Result<PrContext, WatchError> {
        self.record("currentPr");
        Ok(self.current.with_number(pr.unwrap_or(self.current.number)))
    }

    fn pull_request(&self, context: &PrContext) -> Result<PullRequestFacts, WatchError> {
        self.record("pullRequest");
        let facts = PullRequestFacts {
            context: context.clone(),
            ..self.facts.clone()
        };
        match &self.on_pull_request {
            Some(hook) => hook(context, facts),
            None => Ok(facts),
        }
    }

    fn open_pull_requests(&self, _context: &PrContext) -> Result<Vec<OpenPullRequest>, WatchError> {
        self.record("openPullRequests");
        Ok(self.open_pull_requests.clone())
    }

    fn checks_fast_path(&self, _context: &PrContext) -> Result<ChecksFastPath, WatchError> {
        self.record("checksFastPath");
        Ok(self.fast_path.clone())
    }

    fn check_rollup_page(
        &self,
        _context: &PrContext,
        after: Option<&str>,
    ) -> Result<RollupPage, WatchError> {
        self.record(format!("checkRollupPage:{}", cursor(after)));
        Ok(Self::next(&self.rollup_pages, &self.page).unwrap_or_default())
    }

    fn review_thread_page(
        &self,
        _context: &PrContext,
        after: Option<&str>,
    ) -> Result<ReviewThreadPage, WatchError> {
        self.record(format!("reviewThreadPage:{}", cursor(after)));
        Ok(
            Self::next(&self.thread_pages, &self.thread_page).unwrap_or_else(|| ReviewThreadPage {
                threads: self
                    .threads
                    .iter()
                    .map(|thread| RawReviewThread {
                        id: thread.id.clone(),
                        first_comment: thread.first_comment.clone(),
                        resolved: false,
                    })
                    .collect(),
                end_cursor: None,
            }),
        )
    }

    fn commit_rollups(&self, _context: &PrContext) -> Result<Vec<CommitRollup>, WatchError> {
        self.record("commitRollups");
        Ok(self.commit_rollups.clone())
    }
}
