//! The filesystem watcher.
//!
//! Caudra edits the same tree the workbench is showing, so panes that only
//! refreshed on `F5` would quietly disagree with the disk while the agent
//! works. The watcher turns that churn into three facts the panes can act on:
//! which files were written, whether the tree changed shape, and whether the
//! repository's own state moved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use flume::Receiver;
use ignore::WalkBuilder;
use notify::event::ModifyKind;
use notify::{
    ErrorKind, Event, EventKind, RecommendedWatcher, RecursiveMode, Result as Watched, Watcher as _,
};
use tracing::{info, warn};

use crate::fs::tree::GIT_DIR;

/// How long the tree has to stay quiet before a burst is reported.
const SETTLE: Duration = Duration::from_millis(200);
const REGISTER_THREAD_NAME: &str = "workbench-watch";
const REFS_DIR: &str = "refs";
const OBJECTS_DIR: &str = "objects";
const LOGS_DIR: &str = "logs";

#[derive(Default)]
struct GitMetadata {
    roots: Vec<PathBuf>,
}

impl GitMetadata {
    fn discover(root: &Path) -> Self {
        let Ok(repo) = gix::discover(root) else {
            return Self::default();
        };
        let mut roots: Vec<_> = [repo.git_dir(), repo.common_dir()]
            .into_iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        roots.sort();
        roots.dedup();
        Self { roots }
    }

    fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    fn ignored(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| {
            path.strip_prefix(root)
                .ok()
                .and_then(|relative| relative.components().next())
                .is_some_and(|part| part.as_os_str() == OBJECTS_DIR || part.as_os_str() == LOGS_DIR)
        })
    }

    fn register(&self, watcher: &mut RecommendedWatcher) -> usize {
        self.roots
            .iter()
            .map(|root| {
                let shallow = watcher.watch(root, RecursiveMode::NonRecursive).is_ok();
                let refs = watcher
                    .watch(&root.join(REFS_DIR), RecursiveMode::Recursive)
                    .is_ok();
                usize::from(shallow) + usize::from(refs)
            })
            .sum()
    }

    fn update_watches(&self, watcher: &mut RecommendedWatcher, event: &Event) {
        if !reshapes(&event.kind) {
            return;
        }
        for root in &self.roots {
            let refs = root.join(REFS_DIR);
            if event.paths.contains(&refs) {
                let _ = watcher.unwatch(&refs);
                let _ = watcher.watch(&refs, RecursiveMode::Recursive);
            }
        }
        for path in &event.paths {
            let Some(parent) = path.parent() else {
                continue;
            };
            if !path.is_dir() {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(parent) else {
                continue;
            };
            if !self.contains(&canonical)
                && self.roots.iter().any(|root| root.starts_with(&canonical))
                && subtrees(parent).contains(path)
            {
                for (dir, mode) in source_watches(vec![path.clone()], self) {
                    let _ = watcher.watch(&dir, mode);
                }
            }
        }
    }
}

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

/// A watch over the parts of the root the repository does not ignore.
///
/// Registering an inotify watch costs one `inotify_add_watch` per directory
/// under what is asked for, and `notify` blocks the caller until every one of
/// them is in. A recursive watch on the root of a Rust checkout therefore walks
/// `target`, which holds the overwhelming majority of the directories and none
/// of the source, so the watch is placed on each subtree git tracks instead.
/// Registration then runs on its own thread, so opening the workbench costs
/// nothing that scales with the tree.
///
/// Dropping this stops the watch, so closing the workbench does not leave a
/// thread holding kernel handles for a tree nobody is looking at.
pub struct Watch {
    events: Receiver<Watched<Event>>,
    settle: Settle,
    /// Held only to keep the watch alive; every event arrives on the channel.
    /// Empty until registration hands it over.
    watcher: Option<RecommendedWatcher>,
    git: GitMetadata,
    registered: Receiver<(RecommendedWatcher, GitMetadata)>,
}

impl Watch {
    pub fn start(root: &Path) -> Option<Self> {
        let (sender, events) = flume::unbounded();
        let mut watcher = notify::recommended_watcher(sender).ok()?;
        let (done, registered) = flume::bounded(1);
        let root = root.to_path_buf();
        thread::Builder::new()
            .name(REGISTER_THREAD_NAME.to_owned())
            .spawn(move || {
                let started = Instant::now();
                // The root itself is watched shallowly, so an entry appearing
                // beside the subtrees is still noticed without pulling in what
                // sits under its neighbours.
                let shallow = watcher.watch(&root, RecursiveMode::NonRecursive).is_ok();
                let git = GitMetadata::discover(&root);
                let metadata_watched = git.register(&mut watcher);
                let sources = source_watches(subtrees(&root), &git);
                let watched = sources
                    .iter()
                    .filter(|(dir, mode)| watcher.watch(dir, *mode).is_ok())
                    .count();
                info!(
                    shallow,
                    subtrees = sources.len(),
                    watched,
                    metadata_watched,
                    register_ms = started.elapsed().as_millis() as u64,
                    "workbench watch started"
                );
                // A workbench closed mid-registration has already dropped the
                // receiver, and the watcher falling out of the refused send is
                // what stops the watch.
                let _ = done.send((watcher, git));
            })
            .ok()?;
        Some(Self {
            events,
            settle: Settle::default(),
            watcher: None,
            git: GitMetadata::default(),
            registered,
        })
    }

    /// Whether the watch is live. Registration runs on its own thread, so a
    /// change made before this holds is one the watch never saw.
    pub fn is_live(&mut self) -> bool {
        if let Ok((watcher, git)) = self.registered.try_recv() {
            self.watcher = Some(watcher);
            self.settle.absorb(
                Changes {
                    git: !git.roots.is_empty(),
                    ..Changes::default()
                },
                Instant::now(),
            );
            self.git = git;
        }
        self.watcher.is_some()
    }

    /// Empty until the tree has been quiet for [`SETTLE`], so a caller can ask
    /// on every frame without paying for a walk on every write.
    pub fn drain(&mut self) -> Changes {
        if !self.is_live() {
            return Changes::default();
        }
        let now = Instant::now();
        let events = self.events.try_iter().flatten().inspect(|event| {
            if let Some(watcher) = &mut self.watcher {
                self.git.update_watches(watcher, event);
            }
        });
        self.settle.absorb(fold(events, Some(&self.git)), now);
        self.settle.take(now)
    }
}

/// What became of a directory [`HostWatch::sync`] was asked to watch.
enum Placement {
    Watched,
    /// Absent when last asked, so each sync spends one `stat` on whether it
    /// has appeared since.
    Missing,
    /// Present but refused, for permissions or the watch limit, and not asked
    /// again while it stays listed.
    Refused,
}

/// A watch over directories outside the project: Caudra's own, the session's
/// scratch space, a folder the user added.
///
/// These sit wherever the machine keeps them, a home directory among them, so
/// each is watched on its own and never recursively. Every watch is then a
/// single `inotify_add_watch`, which leaves nothing for a registration thread
/// to do. A repository one of them belongs to is not the project's, so nothing
/// here asks git anything and [`Changes::git`] is never set.
///
/// Paths are reported as `notify` spells them under the directory passed to
/// [`HostWatch::sync`], so a caller that keys tabs by path should pass
/// directories spelled the way those keys are.
///
/// Dropping this stops every watch.
pub struct HostWatch {
    events: Receiver<Watched<Event>>,
    settle: Settle,
    /// Held only to keep the watches alive; every event arrives on the channel.
    watcher: RecommendedWatcher,
    placements: HashMap<PathBuf, Placement>,
}

impl HostWatch {
    pub fn start() -> Option<Self> {
        let (sender, events) = flume::unbounded();
        let watcher = notify::recommended_watcher(sender).ok()?;
        Some(Self {
            events,
            settle: Settle::default(),
            watcher,
            placements: HashMap::new(),
        })
    }

    /// Watches exactly `dirs` from now on. A directory already watched or
    /// already refused costs nothing, so this can run whenever the listing
    /// might have changed.
    pub fn sync(&mut self, dirs: &HashSet<PathBuf>) {
        self.placements.retain(|dir, placement| {
            let listed = dirs.contains(dir);
            if !listed && matches!(placement, Placement::Watched) {
                let _ = self.watcher.unwatch(dir);
            }
            listed
        });
        for dir in dirs {
            let retry = match self.placements.get(dir) {
                None => true,
                Some(Placement::Missing) => dir.is_dir(),
                Some(Placement::Watched | Placement::Refused) => false,
            };
            if retry {
                self.placements
                    .insert(dir.clone(), place(&mut self.watcher, dir));
            }
        }
    }

    /// Empty until the directories have been quiet for [`SETTLE`], like
    /// [`Watch::drain`].
    pub fn drain(&mut self) -> Changes {
        let now = Instant::now();
        let events: Vec<_> = self.events.try_iter().flatten().collect();
        // The kernel drops a watch along with its directory, and one made again
        // in its place is a new directory to watch, so a listed entry that
        // appeared, vanished or moved is placed afresh.
        for event in events.iter().filter(|event| reshapes(&event.kind)) {
            for path in &event.paths {
                let Some(placement) = self.placements.get_mut(path) else {
                    continue;
                };
                if matches!(placement, Placement::Watched) {
                    let _ = self.watcher.unwatch(path);
                }
                *placement = place(&mut self.watcher, path);
            }
        }
        self.settle.absorb(fold(events.into_iter(), None), now);
        self.settle.take(now)
    }
}

/// Watches `dir` itself, never what sits under it.
fn place(watcher: &mut RecommendedWatcher, dir: &Path) -> Placement {
    match watcher.watch(dir, RecursiveMode::NonRecursive) {
        Ok(()) => Placement::Watched,
        Err(error) if matches!(error.kind, ErrorKind::PathNotFound) => Placement::Missing,
        Err(error) => {
            warn!(%error, "workbench host directory refused a watch");
            Placement::Refused
        }
    }
}

/// The directories under `root` the repository does not ignore, which are the
/// ones a watch is worth placing on. Dotted directories are kept, since
/// `.github` and `.cargo` hold source; `.git` is dropped, since watching it
/// reports every loose object git writes. `WalkBuilder`'s defaults already
/// apply the ignore rules, the global excludes and the parents above the root.
fn subtrees(root: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(root)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != GIT_DIR)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != root && entry.file_type().is_some_and(|kind| kind.is_dir()))
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

fn source_watches(mut pending: Vec<PathBuf>, git: &GitMetadata) -> Vec<(PathBuf, RecursiveMode)> {
    let mut watches = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(canonical) = fs::canonicalize(&dir) else {
            continue;
        };
        if git.contains(&canonical) {
            continue;
        }
        let mode = if git.roots.iter().any(|root| root.starts_with(&canonical)) {
            pending.extend(subtrees(&dir));
            RecursiveMode::NonRecursive
        } else {
            RecursiveMode::Recursive
        };
        watches.push((dir, mode));
    }
    watches
}

/// Whether an entry appeared, vanished or moved, rather than changed in place.
fn reshapes(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Kept apart from the watch so the classification can be tested against
/// events built by hand rather than against a real tree and a sleep.
///
/// `git` is the project's repository. Without one, as for a directory outside
/// the project, paths under `.git` are dropped rather than reported as
/// repository state, since no pane shows that repository.
fn fold(events: impl Iterator<Item = Event>, git: Option<&GitMetadata>) -> Changes {
    let mut changes = Changes::default();
    for event in events {
        let structural = reshapes(&event.kind);
        if !structural && !matches!(event.kind, EventKind::Modify(_)) {
            continue;
        }
        for path in event.paths {
            if git.is_some_and(|git| git.ignored(&path)) {
                continue;
            }
            if in_git_dir(&path) || git.is_some_and(|git| git.contains(&path)) {
                changes.git |= git.is_some();
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
    use std::collections::HashSet;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    use notify::event::{CreateKind, MetadataKind, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind, RecursiveMode, Result as Watched};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Changes, GitMetadata, HostWatch, SETTLE, Settle, Watch, fold, source_watches, subtrees,
    };

    /// Bounds a failure rather than pacing a success: a working watch answers in
    /// tens of milliseconds and the test ends there, while a broken one is only
    /// distinguishable from a slow one by giving up eventually.
    const DELIVERY_DEADLINE: Duration = Duration::from_secs(10);
    /// Short enough that the test ends as soon as the burst settles.
    const POLL: Duration = Duration::from_millis(10);

    const NOT_LISTED: &str = "a path that changed is missing from the drained set";
    const WRONG_SHAPE: &str = "the tree was told the wrong thing about its shape";
    const GIT_LEAKED: &str =
        "a path under .git was reported as a file rather than as repository state";
    const TOO_EAGER: &str = "a burst was reported before the tree went quiet";
    const HELD_BACK: &str = "a settled burst was not reported";
    const WRONG_TREES: &str =
        "the watch must cover the source trees and skip .git and whatever the repository ignores";
    const NEVER_LIVE: &str = "the watch never finished registering";
    const NOT_DELIVERED: &str = "a write under a watched subtree never reached the drain";
    const METADATA: &str = "metadata";
    const PROJECT: &str = "project";
    const HEAD: &str = "HEAD";
    const BRANCH: &str = "refs/heads/main";
    const HEAD_CONTENT: &str = "ref: refs/heads/main\n";
    const OBJECT_ID: &str = "0123456789012345678901234567890123456789\n";
    const WRONG_METADATA: &str = "repository metadata paths were not resolved or deduplicated";
    const OBJECT_CHURN: &str = "object or reflog churn must not refresh repository state";
    const STORAGE: &str = "storage";
    const SOURCE: &str = "source";
    const NEW_SOURCE: &str = "new-source";
    const SOURCE_FILE: &str = "new.rs";
    const SOURCE_CONTENT: &str = "fn main() {}\n";
    #[cfg(unix)]
    const WORKTREE_ALIAS: &str = "alias";
    const SENTINEL_FILE: &str = "sentinel.rs";
    const KEPT: &str = "kept";
    const DROPPED: &str = "dropped";
    const NESTED: &str = "nested";
    const LATER: &str = "later";
    const STILL_WATCHED: &str = "a directory dropped from the set was still reported";
    const TOO_DEEP: &str = "a host directory was watched recursively";
    const GIT_REFRESHED: &str = "a directory outside the project refreshed source control";

    const CONTENT: EventKind = EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any));
    const CREATED: EventKind = EventKind::Create(CreateKind::File);
    const REMOVED: EventKind = EventKind::Remove(RemoveKind::File);
    const RENAMED: EventKind = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
    const CHMOD: EventKind = EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions));

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    fn drained(events: Vec<Event>) -> Changes {
        fold(events.into_iter(), Some(&GitMetadata::default()))
    }

    fn external_repository() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(PROJECT);
        fs::create_dir(&root).unwrap();
        gix::init(&root).unwrap();
        let metadata = dir.path().join(METADATA);
        fs::rename(root.join(super::GIT_DIR), &metadata).unwrap();
        fs::write(
            root.join(super::GIT_DIR),
            format!("gitdir: {}\n", metadata.display()),
        )
        .unwrap();
        dir
    }

    fn live_watch(root: &Path) -> Watch {
        let mut watch = Watch::start(root).unwrap();
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        while !watch.is_live() {
            assert!(Instant::now() < deadline, "{NEVER_LIVE}");
            thread::yield_now();
        }
        git_change(&mut watch);
        watch
    }

    fn git_change(watch: &mut Watch) {
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        loop {
            let changes = watch.drain();
            assert!(changes.files.is_empty(), "{GIT_LEAKED}");
            assert!(!changes.structural, "{WRONG_SHAPE}");
            if changes.git {
                return;
            }
            assert!(Instant::now() < deadline, "{NOT_DELIVERED}");
            thread::yield_now();
        }
    }

    fn file_change(watch: &mut Watch, path: &Path) {
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        loop {
            if watch.drain().files.contains(path) {
                return;
            }
            assert!(Instant::now() < deadline, "{NOT_DELIVERED}");
            thread::yield_now();
        }
    }

    fn listed(dirs: &[&Path]) -> HashSet<PathBuf> {
        dirs.iter().map(|dir| dir.to_path_buf()).collect()
    }

    fn host_watch(dirs: &[&Path]) -> HostWatch {
        let mut watch = HostWatch::start().unwrap();
        watch.sync(&listed(dirs));
        watch
    }

    /// Everything drained until `path` shows up. A single inotify queue keeps
    /// its events in order, so a path written before `path` and missing from
    /// this was never reported, which is how a test proves a negative without
    /// waiting out a guess.
    fn host_change(watch: &mut HostWatch, path: &Path) -> Changes {
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        let mut seen = Changes::default();
        loop {
            seen.merge(watch.drain());
            if seen.files.contains(path) {
                return seen;
            }
            assert!(Instant::now() < deadline, "{NOT_DELIVERED}");
            thread::sleep(POLL);
        }
    }

    #[test_case("HEAD", CONTENT ; "head")]
    #[test_case("index", RENAMED ; "index_replacement")]
    #[test_case("packed-refs", RENAMED ; "packed_refs_replacement")]
    #[test_case("refs/heads/main", CREATED ; "new_branch")]
    #[test_case("refs/heads/main", REMOVED ; "removed_branch")]
    #[test_case("refs/heads/main", RENAMED ; "branch_replacement")]
    fn external_metadata_events_only_refresh_git(relative: &str, kind: EventKind) {
        let root = PathBuf::from(METADATA);
        let changes = fold(
            [Event::new(kind).add_path(root.join(relative))].into_iter(),
            Some(&GitMetadata { roots: vec![root] }),
        );
        assert!(changes.git, "{NOT_DELIVERED}");
        assert!(changes.files.is_empty(), "{GIT_LEAKED}");
        assert!(!changes.structural, "{WRONG_SHAPE}");
    }

    #[test_case("objects/ab/cdef" ; "loose_object")]
    #[test_case("objects/pack/new.pack" ; "pack")]
    #[test_case("logs/HEAD" ; "reflog")]
    fn object_and_reflog_events_do_not_refresh_git(relative: &str) {
        let root = PathBuf::from(super::GIT_DIR);
        let changes = fold(
            [Event::new(CREATED).add_path(root.join(relative))].into_iter(),
            Some(&GitMetadata { roots: vec![root] }),
        );
        assert!(changes.is_empty(), "{OBJECT_CHURN}");
    }

    #[test_case(false ; "repository_root")]
    #[test_case(true ; "repository_subdirectory")]
    fn metadata_discovery_deduplicates_normal_repository_paths(nested: bool) {
        let dir = TempDir::new().unwrap();
        gix::init(dir.path()).unwrap();
        let root = if nested {
            let root = dir.path().join(PROJECT);
            fs::create_dir(&root).unwrap();
            root
        } else {
            dir.path().to_path_buf()
        };
        assert_eq!(
            GitMetadata::discover(&root).roots,
            vec![fs::canonicalize(dir.path().join(super::GIT_DIR)).unwrap()],
            "{WRONG_METADATA}"
        );
    }

    #[test]
    fn external_metadata_is_discovered_and_watched() {
        let dir = external_repository();
        let root = dir.path().join(PROJECT);
        let metadata = dir.path().join(METADATA);
        assert_eq!(
            GitMetadata::discover(&root).roots,
            vec![fs::canonicalize(&metadata).unwrap()],
            "{WRONG_METADATA}"
        );
        let mut watch = live_watch(&root);
        fs::write(metadata.join(HEAD), HEAD_CONTENT).unwrap();
        git_change(&mut watch);
    }

    #[test_case(false ; "direct_metadata")]
    #[test_case(true ; "nested_metadata")]
    fn source_watches_exclude_external_metadata_without_losing_siblings(nested: bool) {
        let dir = external_repository();
        let root = dir.path().join(PROJECT);
        let parent = if nested {
            let parent = root.join(STORAGE);
            fs::create_dir(&parent).unwrap();
            parent
        } else {
            root.clone()
        };
        let metadata = parent.join(METADATA);
        fs::rename(dir.path().join(METADATA), &metadata).unwrap();
        fs::write(
            root.join(super::GIT_DIR),
            format!("gitdir: {}\n", metadata.display()),
        )
        .unwrap();
        let source = parent.join(SOURCE);
        fs::create_dir(&source).unwrap();
        let mut watched = source_watches(subtrees(&root), &GitMetadata::discover(&root));
        watched.sort_by(|a, b| a.0.cmp(&b.0));
        let new_source = parent.join(NEW_SOURCE);
        let expected = if nested {
            vec![
                (parent, RecursiveMode::NonRecursive),
                (source, RecursiveMode::Recursive),
            ]
        } else {
            vec![(source, RecursiveMode::Recursive)]
        };
        assert_eq!(watched, expected, "{OBJECT_CHURN}");
        let mut watch = live_watch(&root);
        fs::create_dir(&new_source).unwrap();
        file_change(&mut watch, &new_source);
        let file = new_source.join(SOURCE_FILE);
        fs::write(&file, SOURCE_CONTENT).unwrap();
        file_change(&mut watch, &file);
    }

    #[cfg(unix)]
    #[test]
    fn source_watches_compare_canonical_metadata_paths() {
        let dir = external_repository();
        let root = dir.path().join(PROJECT);
        let metadata = root.join(METADATA);
        fs::rename(dir.path().join(METADATA), &metadata).unwrap();
        fs::write(
            root.join(super::GIT_DIR),
            format!("gitdir: {}\n", metadata.display()),
        )
        .unwrap();
        fs::create_dir(root.join(SOURCE)).unwrap();
        let alias = dir.path().join(WORKTREE_ALIAS);
        symlink(&root, &alias).unwrap();
        assert_eq!(
            source_watches(subtrees(&alias), &GitMetadata::discover(&alias)),
            vec![(alias.join(SOURCE), RecursiveMode::Recursive)],
            "{OBJECT_CHURN}"
        );
    }

    #[test_case(false ; "refs_created_after_registration")]
    #[test_case(true ; "refs_replaced_after_registration")]
    fn a_new_refs_directory_stays_watched(replace: bool) {
        let dir = external_repository();
        let root = dir.path().join(PROJECT);
        let metadata = dir.path().join(METADATA);
        let mut watch = live_watch(&root);
        fs::remove_dir_all(metadata.join(super::REFS_DIR)).unwrap();
        if !replace {
            git_change(&mut watch);
        }
        let branch = metadata.join(BRANCH);
        fs::create_dir_all(branch.parent().unwrap()).unwrap();
        fs::write(&branch, OBJECT_ID).unwrap();
        git_change(&mut watch);
        fs::write(&branch, OBJECT_ID).unwrap();
        git_change(&mut watch);
    }

    #[test]
    fn registration_reconciles_git_once_and_keeps_early_events() {
        let (sender, events) = flume::unbounded();
        let (done, registered) = flume::bounded(1);
        let mut watch = Watch {
            events,
            settle: Settle::default(),
            watcher: None,
            git: GitMetadata::default(),
            registered,
        };
        let metadata = PathBuf::from(METADATA);
        sender
            .send(Ok(Event::new(CONTENT).add_path(metadata.join(HEAD))))
            .unwrap();
        assert!(watch.drain().is_empty(), "{TOO_EAGER}");
        assert_eq!(watch.events.len(), 1, "{NOT_DELIVERED}");
        done.send((
            notify::recommended_watcher(|_: Watched<Event>| {}).unwrap(),
            GitMetadata {
                roots: vec![metadata],
            },
        ))
        .unwrap();
        assert!(watch.is_live(), "{NEVER_LIVE}");
        assert!(watch.settle.pending.git, "{NOT_DELIVERED}");
        assert!(watch.drain().is_empty(), "{TOO_EAGER}");
        let changes = watch.settle.take(Instant::now() + SETTLE);
        assert!(changes.git, "{NOT_DELIVERED}");
        assert!(changes.files.is_empty(), "{GIT_LEAKED}");
        assert!(watch.is_live(), "{NEVER_LIVE}");
        assert!(
            watch.settle.take(Instant::now() + SETTLE).is_empty(),
            "{HELD_BACK}"
        );
        assert!(watch.drain().is_empty(), "{HELD_BACK}");
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

    /// The watch is now a set of subtrees rather than one recursive watch on
    /// the root, and nothing else covers the case where that set is registered
    /// but delivers nothing: the panes would simply stop noticing the disk.
    #[test]
    fn a_write_under_a_watched_subtree_is_delivered() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        let mut watch = Watch::start(root).expect("a watch over a plain directory");

        let deadline = Instant::now() + DELIVERY_DEADLINE;
        while !watch.is_live() {
            assert!(Instant::now() < deadline, "{NEVER_LIVE}");
            thread::yield_now();
        }

        let path = root.join("src/a.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let delivered = loop {
            let changes = watch.drain();
            if !changes.is_empty() {
                break changes;
            }
            assert!(Instant::now() < deadline, "{NOT_DELIVERED}");
            thread::sleep(POLL);
        };

        assert!(delivered.files.contains(&path), "{NOT_DELIVERED}");
    }

    /// `target` is the whole reason the watch stopped being recursive: it holds
    /// far more directories than the source does and none of them are source.
    #[test]
    fn only_the_trees_the_repository_keeps_are_watched() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join(".github")).unwrap();
        fs::write(root.join(".gitignore"), "target\n").unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();

        let mut watched: Vec<String> = subtrees(root)
            .iter()
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        watched.sort();

        assert_eq!(watched, vec![".github", "src"], "{WRONG_TREES}");
    }

    #[test]
    fn a_quiet_tree_never_starts_the_clock() {
        let start = Instant::now();
        let mut settle = Settle::default();
        settle.absorb(Changes::default(), start);
        assert!(settle.take(start + SETTLE * 2).is_empty(), "{TOO_EAGER}");
    }

    #[test]
    fn without_a_repository_git_paths_are_dropped() {
        let changes = fold(
            [
                event(CONTENT, "/root/.git/index"),
                event(CREATED, "/root/.git/refs/heads/main"),
            ]
            .into_iter(),
            None,
        );
        assert!(changes.is_empty(), "{GIT_REFRESHED}");
    }

    #[test]
    fn a_write_in_a_host_directory_is_listed_without_disturbing_the_tree() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join(SOURCE_FILE);
        fs::write(&file, SOURCE_CONTENT).unwrap();
        let mut watch = host_watch(&[tmp.path()]);
        fs::write(&file, SOURCE_CONTENT).unwrap();
        let changes = host_change(&mut watch, &file);
        assert!(!changes.structural, "{WRONG_SHAPE}");
        assert!(!changes.git, "{GIT_REFRESHED}");
    }

    #[test_case(false ; "created")]
    #[test_case(true ; "removed")]
    fn an_entry_appearing_or_leaving_a_host_directory_makes_the_tree_stale(remove: bool) {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join(SOURCE_FILE);
        if remove {
            fs::write(&file, SOURCE_CONTENT).unwrap();
        }
        let mut watch = host_watch(&[tmp.path()]);
        if remove {
            fs::remove_file(&file).unwrap();
        } else {
            fs::write(&file, SOURCE_CONTENT).unwrap();
        }
        assert!(host_change(&mut watch, &file).structural, "{WRONG_SHAPE}");
    }

    #[test]
    fn a_directory_dropped_from_the_set_is_no_longer_reported() {
        let tmp = TempDir::new().unwrap();
        let kept = tmp.path().join(KEPT);
        let dropped = tmp.path().join(DROPPED);
        fs::create_dir(&kept).unwrap();
        fs::create_dir(&dropped).unwrap();
        let mut watch = host_watch(&[&kept, &dropped]);
        let early = dropped.join(SOURCE_FILE);
        fs::write(&early, SOURCE_CONTENT).unwrap();
        host_change(&mut watch, &early);

        watch.sync(&listed(&[&kept]));
        let late = dropped.join(SENTINEL_FILE);
        fs::write(&late, SOURCE_CONTENT).unwrap();
        let sentinel = kept.join(SENTINEL_FILE);
        fs::write(&sentinel, SOURCE_CONTENT).unwrap();
        assert!(
            !host_change(&mut watch, &sentinel).files.contains(&late),
            "{STILL_WATCHED}"
        );
    }

    #[test]
    fn a_host_directory_is_not_watched_recursively() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join(NESTED);
        fs::create_dir(&nested).unwrap();
        let mut watch = host_watch(&[tmp.path()]);
        let deep = nested.join(SOURCE_FILE);
        fs::write(&deep, SOURCE_CONTENT).unwrap();
        let sentinel = tmp.path().join(SENTINEL_FILE);
        fs::write(&sentinel, SOURCE_CONTENT).unwrap();
        assert!(
            !host_change(&mut watch, &sentinel).files.contains(&deep),
            "{TOO_DEEP}"
        );
    }

    #[test]
    fn a_directory_missing_at_sync_is_watched_once_it_exists() {
        let tmp = TempDir::new().unwrap();
        let later = tmp.path().join(LATER);
        let mut watch = host_watch(&[&later]);
        fs::create_dir(&later).unwrap();
        watch.sync(&listed(&[&later]));
        let file = later.join(SOURCE_FILE);
        fs::write(&file, SOURCE_CONTENT).unwrap();
        host_change(&mut watch, &file);
    }

    /// The kernel drops a watch along with its directory, so a directory
    /// removed and made again would otherwise fall silent while still listed.
    #[test]
    fn a_recreated_host_directory_is_watched_again() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join(NESTED);
        fs::create_dir(&nested).unwrap();
        let mut watch = host_watch(&[tmp.path(), &nested]);
        fs::remove_dir(&nested).unwrap();
        host_change(&mut watch, &nested);
        fs::create_dir(&nested).unwrap();
        host_change(&mut watch, &nested);
        let file = nested.join(SOURCE_FILE);
        fs::write(&file, SOURCE_CONTENT).unwrap();
        host_change(&mut watch, &file);
    }

    #[test]
    fn a_repository_outside_the_project_never_refreshes_git() {
        let tmp = TempDir::new().unwrap();
        let repository = tmp.path();
        let mut watch = host_watch(&[repository]);
        gix::init(repository).unwrap();
        let metadata = repository.join(super::GIT_DIR);
        watch.sync(&listed(&[repository, &metadata]));
        fs::write(metadata.join(HEAD), HEAD_CONTENT).unwrap();
        let sentinel = repository.join(SENTINEL_FILE);
        fs::write(&sentinel, SOURCE_CONTENT).unwrap();
        let changes = host_change(&mut watch, &sentinel);
        assert!(!changes.git, "{GIT_REFRESHED}");
        assert!(
            changes
                .files
                .iter()
                .all(|path| !path.starts_with(&metadata)),
            "{GIT_LEAKED}"
        );
    }
}
