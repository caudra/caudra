//! The filesystem watcher.
//!
//! Caudra edits the same tree the workbench is showing, so panes that only
//! refreshed on `F5` would quietly disagree with the disk while the agent
//! works. The watcher turns that churn into three facts the panes can act on:
//! which files were written, whether the tree changed shape, and whether the
//! repository's own state moved.

use std::collections::HashSet;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flume::Receiver;
use notify::event::ModifyKind;
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as Watched, Watcher as _,
};

use crate::fs::tree::GIT_DIR;

/// How long the tree has to stay quiet before a burst is reported.
const SETTLE: Duration = Duration::from_millis(200);

/// What moved under the root since the last drain.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Paths outside `.git` that were written, created or removed.
    pub files: HashSet<PathBuf>,
    /// Set when an entry appeared, vanished or was renamed, so the tree has to
    /// be reread rather than only remarked.
    pub structural: bool,
    /// Set when the repository's own state moved, which is what makes a
    /// staged-versus-working listing stale.
    pub git: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && !self.structural && !self.git
    }

    fn merge(&mut self, other: Self) {
        self.files.extend(other.files);
        self.structural |= other.structural;
        self.git |= other.git;
    }
}

/// Holds a burst back until it stops. A build or a `git checkout` writes
/// thousands of paths, and rereading every pane for each of them would cost far
/// more than the answer is worth, so the panes catch up once instead.
#[derive(Default)]
struct Settle {
    pending: Changes,
    /// When the last event landed, and so what the quiet is measured from.
    since: Option<Instant>,
}

impl Settle {
    fn absorb(&mut self, fresh: Changes, now: Instant) {
        if fresh.is_empty() {
            return;
        }
        self.pending.merge(fresh);
        self.since = Some(now);
    }

    fn take(&mut self, now: Instant) -> Changes {
        if self
            .since
            .is_none_or(|since| now.duration_since(since) < SETTLE)
        {
            return Changes::default();
        }
        self.since = None;
        mem::take(&mut self.pending)
    }
}

/// A recursive watch over the workbench root. Dropping it stops the watch, so
/// closing the workbench does not leave a thread holding kernel handles for a
/// tree nobody is looking at.
pub struct Watch {
    events: Receiver<Watched<Event>>,
    settle: Settle,
    /// Held only to keep the watch alive; every event arrives on the channel.
    _watcher: RecommendedWatcher,
}

impl Watch {
    /// A tree that cannot be watched is not worth an error. The panes still
    /// refresh on demand, so the workbench opens either way.
    pub fn start(root: &Path) -> Option<Self> {
        let (sender, events) = flume::unbounded();
        let mut watcher = notify::recommended_watcher(sender).ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        Some(Self {
            events,
            settle: Settle::default(),
            _watcher: watcher,
        })
    }

    /// Empty until the tree has been quiet for [`SETTLE`], so a caller can ask
    /// on every frame without paying for a walk on every write.
    pub fn drain(&mut self) -> Changes {
        let now = Instant::now();
        self.settle
            .absorb(fold(self.events.try_iter().flatten()), now);
        self.settle.take(now)
    }
}

/// Kept apart from the watch so the classification can be tested against
/// events built by hand rather than against a real tree and a sleep.
fn fold(events: impl Iterator<Item = Event>) -> Changes {
    let mut changes = Changes::default();
    for event in events {
        let structural = match event.kind {
            EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_)) => true,
            EventKind::Modify(_) => false,
            _ => continue,
        };
        for path in event.paths {
            if in_git_dir(&path) {
                changes.git = true;
                continue;
            }
            changes.structural |= structural;
            changes.files.insert(path);
        }
    }
    changes
}

/// One commit rewrites a dozen paths under `.git`, and every one of them means
/// the same thing to the panes, so they collapse into a single flag.
fn in_git_dir(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == GIT_DIR)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use notify::event::{CreateKind, MetadataKind, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind};
    use test_case::test_case;

    use super::{Changes, SETTLE, Settle, fold};

    const NOT_LISTED: &str = "a path that changed is missing from the drained set";
    const WRONG_SHAPE: &str = "the tree was told the wrong thing about its shape";
    const GIT_LEAKED: &str =
        "a path under .git was reported as a file rather than as repository state";
    const TOO_EAGER: &str = "a burst was reported before the tree went quiet";
    const HELD_BACK: &str = "a settled burst was not reported";

    const CONTENT: EventKind = EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any));
    const CREATED: EventKind = EventKind::Create(CreateKind::File);
    const REMOVED: EventKind = EventKind::Remove(RemoveKind::File);
    const RENAMED: EventKind = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
    const CHMOD: EventKind = EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions));

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    fn drained(events: Vec<Event>) -> Changes {
        fold(events.into_iter())
    }

    #[test]
    fn a_written_file_is_listed_without_disturbing_the_tree() {
        let changes = drained(vec![event(CONTENT, "/root/a.rs")]);
        assert!(
            changes.files.contains(&PathBuf::from("/root/a.rs")),
            "{NOT_LISTED}"
        );
        assert!(!changes.structural, "{WRONG_SHAPE}");
        assert!(!changes.git, "{GIT_LEAKED}");
    }

    #[test_case(CREATED ; "created")]
    #[test_case(REMOVED ; "removed")]
    #[test_case(RENAMED ; "renamed")]
    fn an_entry_appearing_or_leaving_makes_the_tree_stale(kind: EventKind) {
        assert!(
            drained(vec![event(kind, "/root/a.rs")]).structural,
            "{WRONG_SHAPE}"
        );
    }

    #[test_case(CONTENT ; "written")]
    #[test_case(CHMOD ; "chmod")]
    fn a_change_in_place_leaves_the_tree_alone(kind: EventKind) {
        assert!(
            !drained(vec![event(kind, "/root/a.rs")]).structural,
            "{WRONG_SHAPE}"
        );
    }

    #[test]
    fn everything_under_the_git_directory_collapses_into_one_flag() {
        let changes = drained(vec![
            event(CONTENT, "/root/.git/index"),
            event(CREATED, "/root/.git/refs/heads/main"),
        ]);
        assert!(changes.git, "{GIT_LEAKED}");
        assert!(changes.files.is_empty(), "{GIT_LEAKED}");
        assert!(!changes.structural, "{WRONG_SHAPE}");
    }

    #[test]
    fn repeated_writes_to_one_file_drain_as_a_single_path() {
        let changes = drained(vec![
            event(CONTENT, "/root/a.rs"),
            event(CONTENT, "/root/a.rs"),
        ]);
        assert_eq!(changes.files.len(), 1, "{NOT_LISTED}");
    }

    #[test]
    fn nothing_happening_drains_as_nothing() {
        assert!(drained(Vec::new()).is_empty(), "{NOT_LISTED}");
        assert!(
            drained(vec![Event::new(EventKind::Access(
                notify::event::AccessKind::Read
            ))])
            .is_empty(),
            "{NOT_LISTED}"
        );
    }

    #[test]
    fn a_burst_is_held_until_the_writing_stops() {
        let start = Instant::now();
        let mut settle = Settle::default();
        settle.absorb(drained(vec![event(CONTENT, "/root/a.rs")]), start);
        assert!(settle.take(start + SETTLE / 2).is_empty(), "{TOO_EAGER}");

        settle.absorb(drained(vec![event(CONTENT, "/root/b.rs")]), start + SETTLE);
        assert!(
            settle.take(start + SETTLE * 3 / 2).is_empty(),
            "{TOO_EAGER}"
        );

        let changes = settle.take(start + SETTLE * 2);
        assert_eq!(changes.files.len(), 2, "{HELD_BACK}");
    }

    #[test]
    fn a_reported_burst_is_not_reported_twice() {
        let start = Instant::now();
        let mut settle = Settle::default();
        settle.absorb(drained(vec![event(CREATED, "/root/a.rs")]), start);

        assert!(settle.take(start + SETTLE).structural, "{HELD_BACK}");
        assert!(settle.take(start + SETTLE * 2).is_empty(), "{HELD_BACK}");
    }

    #[test]
    fn a_quiet_tree_never_starts_the_clock() {
        let start = Instant::now();
        let mut settle = Settle::default();
        settle.absorb(Changes::default(), start);
        assert!(settle.take(start + SETTLE * 2).is_empty(), "{TOO_EAGER}");
    }
}
