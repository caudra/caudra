//! The workspace snapshot a revert needs, captured the first time a tool call
//! might change a file.
//!
//! Capturing before a run instead meant every conversational turn walked and
//! hashed the working tree for a revert point nothing would ever use. The per
//! call effect already says which calls can change the tree, so the capture
//! moves behind the first of them: a session that only reads leaves no store on
//! disk at all, and one that writes still cannot touch a file before the state
//! it is about to overwrite has been recorded.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_storage::id::CaudraId;
use tracing::warn;

use crate::snapshots::{SnapshotError, SnapshotStore};

const SNAPSHOTS_DISABLED: &str = "workspace snapshots are off in your configuration";

/// What a mutating call learns before it runs.
#[derive(Debug)]
pub enum BaselineOutcome {
    /// A revert point for this run exists.
    Ready,
    /// This workspace will not be snapshotted, and the call may proceed anyway.
    /// A deliberate refusal costs file revert, not the user's work.
    Unavailable(Arc<String>),
    /// The capture was attempted and did not finish, so there is no revert
    /// point. The call must not proceed.
    Failed(SnapshotError),
}

impl BaselineOutcome {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// The store and worktree a capture would use. Replaced wholesale by `/cd` and
/// by loading another session, because both change where the baseline lives.
struct BaselineTarget {
    store: Arc<SnapshotStore>,
    cwd: PathBuf,
}

impl BaselineTarget {
    /// Both halves already exist on disk, so there is nothing to capture. Two
    /// `exists` checks, which is what makes calling this before every mutating
    /// call affordable.
    fn is_captured(&self, head: Option<CaudraId>) -> bool {
        self.store.has_session_start() && head.is_none_or(|head| self.store.has_checkpoint(head))
    }

    /// A reused head keeps the snapshot it already has rather than taking a
    /// fresher one: both were taken while that item was the newest, and the
    /// earlier state is the one that undoes more.
    fn capture(&self, head: Option<CaudraId>) -> Result<(), SnapshotError> {
        self.store.snapshot_session_start(&self.cwd)?;
        if let Some(head) = head
            && !self.store.has_checkpoint(head)
        {
            self.store.snapshot(&self.cwd, head)?;
        }
        Ok(())
    }
}

/// One per session, shared with every agent serving it, the way `PathLocks` is.
pub struct WorkspaceBaseline {
    /// From configuration, so it never changes for the life of the process and
    /// `rebind` must not clear it.
    enabled: bool,
    target: ArcSwap<BaselineTarget>,
    /// Single-flights the capture: parallel tool calls and subagents all reach
    /// this, and the second one through must wait rather than start its own.
    gate: async_lock::Mutex<()>,
    /// Sticky, so a workspace the store refused is judged once rather than on
    /// every call.
    unavailable: ArcSwapOption<String>,
}

impl WorkspaceBaseline {
    pub fn new(store: Arc<SnapshotStore>, cwd: PathBuf, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            target: ArcSwap::from_pointee(BaselineTarget { store, cwd }),
            gate: async_lock::Mutex::default(),
            unavailable: ArcSwapOption::empty(),
        })
    }

    /// Points the baseline at another store and worktree. The refusal is dropped
    /// with the old target: a new workspace earns its own verdict.
    pub fn rebind(&self, store: Arc<SnapshotStore>, cwd: PathBuf) {
        self.target.store(Arc::new(BaselineTarget { store, cwd }));
        self.unavailable.store(None);
    }

    /// Why this workspace has no file revert, whether by configuration or by
    /// a refusal earned on the tree itself.
    pub fn unavailable_reason(&self) -> Option<Arc<String>> {
        if !self.enabled {
            return Some(Arc::new(SNAPSHOTS_DISABLED.to_owned()));
        }
        self.unavailable.load_full()
    }

    /// The refusal alone, for the UI to report once. Configuration is not news
    /// worth interrupting anyone over: they chose it.
    pub fn refusal(&self) -> Option<Arc<String>> {
        self.unavailable.load_full()
    }

    /// Whether a capture has landed, so a caller that only wants to refresh an
    /// existing baseline can tell there is nothing to refresh.
    pub fn is_captured(&self) -> bool {
        self.target.load().is_captured(None)
    }

    pub fn cwd(&self) -> PathBuf {
        self.target.load().cwd.clone()
    }

    /// Holds the gate across the capture, so the call that asked second arrives
    /// after the baseline is on disk rather than alongside it.
    pub async fn ensure(&self, head: Option<CaudraId>) -> BaselineOutcome {
        if let Some(reason) = self.unavailable_reason() {
            return BaselineOutcome::Unavailable(reason);
        }
        let _gate = self.gate.lock().await;
        // Re-read under the gate: whoever held it may have just answered this.
        if let Some(reason) = self.unavailable.load_full() {
            return BaselineOutcome::Unavailable(reason);
        }
        let target = self.target.load_full();
        if target.is_captured(head) {
            return BaselineOutcome::Ready;
        }
        let work = Arc::clone(&target);
        match smol::unblock(move || work.capture(head)).await {
            Ok(()) => BaselineOutcome::Ready,
            Err(error) if error.is_workspace_refusal() => {
                warn!(
                    cwd = %target.cwd.display(),
                    %error,
                    "workspace refused for snapshots, file revert is off"
                );
                let reason = Arc::new(error.to_string());
                self.unavailable.store(Some(Arc::clone(&reason)));
                BaselineOutcome::Unavailable(reason)
            }
            Err(error) => BaselineOutcome::Failed(error),
        }
    }
}

/// The baseline plus the head this run started from, which is the item a revert
/// brackets. Carried by `ToolContext` and inherited by subagents, so a child's
/// first write captures the parent run's revert point.
#[derive(Clone)]
pub struct BaselineGate {
    baseline: Arc<WorkspaceBaseline>,
    head: Option<CaudraId>,
}

impl BaselineGate {
    pub fn new(baseline: Arc<WorkspaceBaseline>, head: Option<CaudraId>) -> Self {
        Self { baseline, head }
    }

    pub async fn ensure(&self) -> BaselineOutcome {
        self.baseline.ensure(self.head).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::snapshots::SnapshotLimits;

    const FILE: &str = "tracked.txt";
    const CONTENTS: &str = "alpha";
    const READY_MSG: &str = "a mutating call gets a revert point";
    const UNAVAILABLE_MSG: &str = "a refused workspace lets the call through";

    fn baseline(enabled: bool, limits: SnapshotLimits) -> (TempDir, Arc<WorkspaceBaseline>) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(FILE), CONTENTS).unwrap();
        let store = Arc::new(SnapshotStore::new(temp.path().join("snapshots")).with_limits(limits));
        let baseline = WorkspaceBaseline::new(store, root, enabled);
        (temp, baseline)
    }

    fn head(sequence: u32) -> CaudraId {
        let mut bytes = [0u8; 16];
        bytes[12..].copy_from_slice(&sequence.to_be_bytes());
        CaudraId::from_bytes(bytes)
    }

    #[test]
    fn the_first_capture_writes_the_anchor_and_the_head() {
        let (_temp, baseline) = baseline(true, SnapshotLimits::default());
        assert!(!baseline.is_captured(), "{READY_MSG}");

        let outcome = smol::block_on(baseline.ensure(Some(head(1))));
        assert!(matches!(outcome, BaselineOutcome::Ready), "{READY_MSG}");
        assert!(baseline.is_captured(), "{READY_MSG}");
        let target = baseline.target.load();
        assert!(target.store.has_checkpoint(head(1)), "{READY_MSG}");
    }

    /// Parallel tool calls all reach the gate, and a capture is the expensive
    /// half of a mutating call: doing it twice would double the cost of the
    /// first write in every batch.
    #[test]
    fn concurrent_calls_capture_once() {
        let (_temp, baseline) = baseline(true, SnapshotLimits::default());
        let first = Arc::clone(&baseline);
        let second = Arc::clone(&baseline);

        smol::block_on(async move {
            let (left, right) = futures_lite::future::zip(
                first.ensure(Some(head(1))),
                second.ensure(Some(head(1))),
            )
            .await;
            assert!(matches!(left, BaselineOutcome::Ready), "{READY_MSG}");
            assert!(matches!(right, BaselineOutcome::Ready), "{READY_MSG}");
        });
        let target = baseline.target.load();
        assert_eq!(
            target.store.load_session_start_manifest().unwrap().len(),
            1,
            "{READY_MSG}"
        );
    }

    #[test]
    fn a_disabled_baseline_is_unavailable_without_touching_the_store() {
        let (_temp, baseline) = baseline(false, SnapshotLimits::default());

        let outcome = smol::block_on(baseline.ensure(None));
        assert!(
            matches!(outcome, BaselineOutcome::Unavailable(reason) if reason.contains("off")),
            "{UNAVAILABLE_MSG}"
        );
        assert!(!baseline.is_captured(), "{UNAVAILABLE_MSG}");
        assert!(baseline.unavailable_reason().is_some(), "{UNAVAILABLE_MSG}");
    }

    #[test]
    fn a_workspace_over_the_budget_is_refused_once_and_stays_refused() {
        let (_temp, baseline) = baseline(
            true,
            SnapshotLimits {
                max_files: 0,
                ..SnapshotLimits::default()
            },
        );

        let outcome = smol::block_on(baseline.ensure(None));
        let BaselineOutcome::Unavailable(first) = outcome else {
            panic!("{UNAVAILABLE_MSG}: {outcome:?}");
        };
        let second = smol::block_on(baseline.ensure(None));
        assert!(
            matches!(second, BaselineOutcome::Unavailable(reason) if reason == first),
            "{UNAVAILABLE_MSG}"
        );
        assert_eq!(
            baseline.unavailable_reason(),
            Some(first),
            "{UNAVAILABLE_MSG}"
        );
    }

    #[test]
    fn rebinding_clears_a_refusal() {
        let (temp, baseline) = baseline(
            true,
            SnapshotLimits {
                max_files: 0,
                ..SnapshotLimits::default()
            },
        );
        assert!(
            smol::block_on(baseline.ensure(None)).is_unavailable(),
            "{UNAVAILABLE_MSG}"
        );

        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join(FILE), CONTENTS).unwrap();
        baseline.rebind(
            Arc::new(SnapshotStore::new(temp.path().join("other-snapshots"))),
            other,
        );

        assert!(baseline.unavailable_reason().is_none(), "{READY_MSG}");
        assert!(
            matches!(
                smol::block_on(baseline.ensure(None)),
                BaselineOutcome::Ready
            ),
            "{READY_MSG}"
        );
    }
}
