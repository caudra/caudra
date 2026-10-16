//! Repository history signals: per-file churn, co-change, and a diff-seeded teleport vector.
//!
//! The symbol graph answers "what calls what". History answers "what moves together", which is a
//! different and complementary question: two files with no edge between them can still be the pair
//! that always changes in the same commit.
//!
//! This module reads the object database directly through `gix`. It never shells out to `git`,
//! because that would make the graph depend on a program on `PATH` and on the shell group's
//! permission policy, neither of which this crate has or should acquire.
//!
//! # What history cannot tell you
//!
//! Churn counts commits, not intent. A file rewritten once by a refactor and a file corrected
//! twenty times are not ordered by that number in any way a reader should trust on its own. Renames
//! are not followed, so a path's history begins at its current name. And a shallow clone reports
//! every file as changed exactly once, which is a plausible-looking answer to a question the
//! repository cannot answer: that case is reported as unavailable rather than counted.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

use crate::{model::Facts, rank::Teleport};

/// Bounds on the history walk.
///
/// A walk over an unbounded history is a walk over an attacker- or accident-controlled amount of
/// work, so every axis that can grow has a ceiling and the result says which one it hit.
#[derive(Clone, Copy, Debug)]
pub struct HistoryLimits {
    /// Commits to walk before stopping.
    pub max_commits: usize,
    /// Wall-clock ceiling for the whole walk.
    pub deadline: Duration,
    /// Commits touching more than this many paths contribute churn but no co-change pairs.
    ///
    /// A commit touching a thousand files is a reformat, a license header sweep, or a vendored
    /// import. It emits `n*(n-1)/2` pairs that say only "these files exist", and at a thousand
    /// paths that is half a million pairs of noise that would swamp every real pair.
    pub max_paths_per_commit: usize,
    /// Co-change pairs retained in the result.
    pub max_co_change_pairs: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_commits: 2_000,
            deadline: Duration::from_secs(5),
            max_paths_per_commit: 64,
            max_co_change_pairs: 4_096,
        }
    }
}

/// Why history signals are absent.
///
/// Each variant is reported rather than rendered as zeros. A caller that cannot distinguish "no
/// churn" from "no history" will read the first as a fact about the code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unavailable {
    /// No repository was found at or above the given path.
    NoRepository,
    /// The repository is a shallow clone.
    ///
    /// Every reachable file appears to have been touched by exactly as many commits as the clone
    /// happens to contain, which looks like churn and is an artifact of the fetch depth.
    ShallowClone,
    /// The repository has no commits, or `HEAD` is unborn.
    NoHistory,
    /// The object database could not be read.
    ///
    /// Deliberately carries no detail: the underlying error embeds paths.
    Unreadable,
}

impl Unavailable {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoRepository => "no_repository",
            Self::ShallowClone => "shallow_clone",
            Self::NoHistory => "no_history",
            Self::Unreadable => "unreadable",
        }
    }

    /// A sentence a caller can show verbatim.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NoRepository => "no git repository was found, so history signals are unavailable",
            Self::ShallowClone => {
                "the repository is a shallow clone, so commit counts would reflect fetch depth \
                 rather than history"
            }
            Self::NoHistory => "the repository has no commits, so there is no history to read",
            Self::Unreadable => "the object database could not be read",
        }
    }
}

/// Which bound stopped the walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Truncation {
    /// [`HistoryLimits::max_commits`] was reached.
    CommitLimit,
    /// [`HistoryLimits::deadline`] elapsed.
    Deadline,
}

impl Truncation {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CommitLimit => "commit_limit",
            Self::Deadline => "deadline",
        }
    }
}

/// Two paths that changed in the same commit, and how often.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoChange {
    /// Byte-ordered before [`CoChange::second`], so a pair has one representation.
    pub first: String,
    pub second: String,
    pub commits: u32,
}

/// What the bounded walk observed.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Commits actually walked, which is a floor on the repository's history, not its size.
    pub commits_walked: usize,
    /// Set when a bound stopped the walk before the history ran out.
    ///
    /// Every count below is then a floor rather than a total, and a caller that presents them as
    /// totals is asserting something this walk did not establish.
    pub truncated: Option<Truncation>,
    /// Commits touching each path, keyed by repository-relative forward-slash path.
    pub churn: BTreeMap<String, u32>,
    /// Co-change pairs, most frequent first, then byte order. Bounded by the limits.
    pub co_change: Vec<CoChange>,
    /// Paths changed by the most recent commit walked.
    pub changed_in_head: Vec<String>,
}

/// The result of asking a directory for history.
#[derive(Clone, Debug)]
pub enum Signals {
    Available(History),
    Unavailable(Unavailable),
}

impl Signals {
    /// Commits touching `path`, or `None` when history is unavailable.
    ///
    /// `Some(0)` means the walk saw no commit touching the path within its bounds. It does not mean
    /// the file has never changed.
    #[must_use]
    pub fn churn(&self, path: &str) -> Option<u32> {
        match self {
            Self::Available(history) => Some(history.churn.get(path).copied().unwrap_or(0)),
            Self::Unavailable(_) => None,
        }
    }

    #[must_use]
    pub const fn history(&self) -> Option<&History> {
        match self {
            Self::Available(history) => Some(history),
            Self::Unavailable(_) => None,
        }
    }
}

/// Walks history at `root`, bounded by `limits`.
///
/// `root` is a directory the caller has already authorized. This function performs no confinement
/// of its own and resolves no user-supplied path.
#[must_use]
pub fn signals(root: &Path, limits: HistoryLimits) -> Signals {
    match walk(root, limits) {
        Ok(history) => Signals::Available(history),
        Err(reason) => Signals::Unavailable(reason),
    }
}

fn walk(root: &Path, limits: HistoryLimits) -> Result<History, Unavailable> {
    let repository = gix::discover(root).map_err(|_| Unavailable::NoRepository)?;
    if repository.is_shallow() {
        return Err(Unavailable::ShallowClone);
    }

    let head = repository
        .head_commit()
        .map_err(|_| Unavailable::NoHistory)?;
    let walk = repository
        .rev_walk([head.id])
        .all()
        .map_err(|_| Unavailable::Unreadable)?;

    let started = Instant::now();
    let mut history = History::default();
    let mut pair_counts: HashMap<(String, String), u32> = HashMap::new();
    let mut state = gix::diff::tree::State::default();

    for info in walk {
        if history.commits_walked >= limits.max_commits {
            history.truncated = Some(Truncation::CommitLimit);
            break;
        }
        if started.elapsed() >= limits.deadline {
            history.truncated = Some(Truncation::Deadline);
            break;
        }

        let Ok(info) = info else {
            return Err(Unavailable::Unreadable);
        };
        let Ok(commit) = info.object() else {
            return Err(Unavailable::Unreadable);
        };
        history.commits_walked += 1;

        let changed = changed_paths(&repository, &commit, &mut state)?;
        if history.commits_walked == 1 {
            history.changed_in_head.clone_from(&changed);
        }
        for path in &changed {
            *history.churn.entry(path.clone()).or_insert(0) += 1;
        }
        // A merge commit compared against only its first parent reports the whole side branch as
        // changed. Combined with the path ceiling below, that keeps merges from dominating.
        if changed.len() <= limits.max_paths_per_commit {
            for (index, first) in changed.iter().enumerate() {
                for second in &changed[index + 1..] {
                    *pair_counts
                        .entry((first.clone(), second.clone()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    if history.commits_walked == 0 {
        return Err(Unavailable::NoHistory);
    }

    let mut pairs: Vec<CoChange> = pair_counts
        .into_iter()
        .map(|((first, second), commits)| CoChange {
            first,
            second,
            commits,
        })
        .collect();
    // Descending frequency, then byte order on the pair, so the retained head is the same on every
    // run. Sorting by count alone would let the hash map's iteration order decide what survives the
    // truncation below.
    pairs.sort_by(|left, right| {
        right
            .commits
            .cmp(&left.commits)
            .then_with(|| left.first.cmp(&right.first))
            .then_with(|| left.second.cmp(&right.second))
    });
    pairs.truncate(limits.max_co_change_pairs);
    history.co_change = pairs;

    Ok(history)
}

/// The low-level entry iterator the tree differ takes, over an already-decoded tree.
fn tree_ref<'a>(tree: &'a gix::Tree<'_>) -> gix::objs::TreeRefIter<'a> {
    gix::objs::TreeRefIter::from_bytes(&tree.data, tree.id.kind())
}

/// Paths changed by `commit` relative to its first parent, or its full tree for a root commit.
fn changed_paths(
    repository: &gix::Repository,
    commit: &gix::Commit<'_>,
    state: &mut gix::diff::tree::State,
) -> Result<Vec<String>, Unavailable> {
    let tree = commit.tree().map_err(|_| Unavailable::Unreadable)?;
    let parent = commit
        .parent_ids()
        .next()
        .and_then(|id| id.object().ok())
        .and_then(|object| object.try_into_commit().ok())
        .and_then(|parent| parent.tree().ok());

    let empty = repository.empty_tree();
    let previous = parent.as_ref().unwrap_or(&empty);

    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    // `gix::diff::tree` is a function in the value namespace and a module in the type namespace.
    gix::diff::tree(
        tree_ref(previous),
        tree_ref(&tree),
        &mut *state,
        &repository.objects,
        &mut recorder,
    )
    .map_err(|_| Unavailable::Unreadable)?;

    let mut paths: Vec<String> = recorder
        .records
        .into_iter()
        .filter_map(|change| {
            let raw = match change {
                gix::diff::tree::recorder::Change::Addition {
                    path, entry_mode, ..
                }
                | gix::diff::tree::recorder::Change::Deletion {
                    path, entry_mode, ..
                }
                | gix::diff::tree::recorder::Change::Modification {
                    path, entry_mode, ..
                } => {
                    // Trees are recorded alongside their entries; counting both would charge every
                    // directory the churn of everything beneath it.
                    if entry_mode.is_tree() {
                        return None;
                    }
                    path
                }
            };
            String::from_utf8(raw.into()).ok()
        })
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// A teleport vector seeded with every symbol defined in one of `paths`.
///
/// The concentration is [`crate::rank::SEED_CONCENTRATION`], which is below 1.0 on purpose:
/// sending every walk home to the diff would rank the changed files against each other and discard
/// the surrounding architecture, which is the context a diff is being read for in the first place.
///
/// Returns [`Teleport::Uniform`] when no path matches: seeding on an empty set would put all the
/// mass on nothing and produce a rank vector with no relation to the request.
#[must_use]
pub fn teleport_for_paths(facts: &Facts, paths: &[String]) -> Teleport {
    let changed: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let seeds: Vec<_> = facts
        .definitions
        .iter()
        .filter(|definition| {
            facts
                .file(definition.file)
                .is_some_and(|file| changed.contains(file.path.as_str()))
        })
        .map(|definition| definition.node)
        .collect();

    if seeds.is_empty() {
        Teleport::Uniform
    } else {
        Teleport::Seeded(seeds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Builds a repository with the real `git` binary.
    ///
    /// The tests shell out; the crate does not. Constructing history with `gix` would test the
    /// walker against its own writer, and a shared misunderstanding of the format would pass.
    fn repository(commits: &[&[(&str, &str)]]) -> TempDir {
        let directory = TempDir::new().expect("temp dir");
        let root = directory.path();
        let run = |arguments: &[&str]| {
            let status = Command::new("git")
                .args(arguments)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .expect("git runs");
            assert!(status.status.success(), "git {arguments:?} failed");
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        for commit in commits {
            for (path, contents) in *commit {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("mkdir");
                }
                std::fs::write(full, contents).expect("write");
            }
            run(&["add", "-A"]);
            run(&["commit", "--quiet", "-m", "c"]);
        }
        directory
    }

    #[test]
    fn no_repository_is_reported_not_counted_as_zero() {
        let directory = TempDir::new().expect("temp dir");
        let signals = signals(directory.path(), HistoryLimits::default());
        assert!(matches!(
            signals,
            Signals::Unavailable(Unavailable::NoRepository)
        ));
        assert_eq!(
            signals.churn("anything.rs"),
            None,
            "an unavailable walk must not answer a churn question at all"
        );
    }

    #[test]
    fn churn_counts_commits_touching_each_path() {
        let directory = repository(&[
            &[("a.rs", "one"), ("b.rs", "one")],
            &[("a.rs", "two")],
            &[("a.rs", "three")],
        ]);
        let signals = signals(directory.path(), HistoryLimits::default());
        assert_eq!(signals.churn("a.rs"), Some(3));
        assert_eq!(signals.churn("b.rs"), Some(1));
    }

    #[test]
    fn a_path_never_committed_reads_zero_while_history_is_available() {
        // The distinction the Option carries: zero is an answer, None is a refusal to answer.
        let directory = repository(&[&[("a.rs", "one")]]);
        let signals = signals(directory.path(), HistoryLimits::default());
        assert_eq!(signals.churn("never-existed.rs"), Some(0));
    }

    #[test]
    fn a_shallow_clone_reports_unavailable_rather_than_fetch_depth() {
        let source = repository(&[&[("a.rs", "one")], &[("a.rs", "two")], &[("a.rs", "three")]]);
        let destination = TempDir::new().expect("temp dir");
        let clone = destination.path().join("shallow");
        let status = Command::new("git")
            .args(["clone", "--quiet", "--depth=1", "--no-local"])
            .arg(source.path())
            .arg(&clone)
            .output()
            .expect("git runs");
        assert!(status.status.success(), "shallow clone failed");

        let signals = signals(&clone, HistoryLimits::default());
        assert!(
            matches!(signals, Signals::Unavailable(Unavailable::ShallowClone)),
            "a depth-1 clone would report every file as changed once, which is the fetch depth \
             wearing churn's clothes"
        );
    }

    #[test]
    fn directories_are_not_charged_the_churn_of_their_contents() {
        let directory = repository(&[
            &[("src/a.rs", "one")],
            &[("src/b.rs", "one")],
            &[("src/c.rs", "one")],
        ]);
        let signals = signals(directory.path(), HistoryLimits::default());
        assert_eq!(signals.churn("src"), Some(0), "a tree is not a file");
        assert_eq!(signals.churn("src/a.rs"), Some(1));
    }

    #[test]
    fn co_change_pairs_files_that_move_together() {
        let directory = repository(&[
            &[("a.rs", "1"), ("b.rs", "1")],
            &[("a.rs", "2"), ("b.rs", "2")],
            &[("c.rs", "1")],
        ]);
        let signals = signals(directory.path(), HistoryLimits::default());
        let history = signals.history().expect("history");
        let pair = history
            .co_change
            .iter()
            .find(|pair| pair.first == "a.rs" && pair.second == "b.rs")
            .expect("a.rs and b.rs co-changed");
        assert_eq!(pair.commits, 2);
        assert!(
            !history
                .co_change
                .iter()
                .any(|pair| pair.first == "c.rs" || pair.second == "c.rs"),
            "a file that only ever changed alone has no pair"
        );
    }

    #[test]
    fn a_sweeping_commit_contributes_churn_but_no_pairs() {
        // The gate on the path ceiling. Without it one reformat emits n^2/2 pairs asserting that
        // every file in the repository moves with every other.
        let sweep: Vec<(String, String)> = (0..8)
            .map(|index| (format!("f{index}.rs"), "x".to_owned()))
            .collect();
        let borrowed: Vec<(&str, &str)> = sweep
            .iter()
            .map(|(path, body)| (path.as_str(), body.as_str()))
            .collect();
        let directory = repository(&[&borrowed]);
        let limits = HistoryLimits {
            max_paths_per_commit: 4,
            ..HistoryLimits::default()
        };
        let signals = signals(directory.path(), limits);
        let history = signals.history().expect("history");
        assert_eq!(history.churn.len(), 8, "churn is still counted");
        assert!(
            history.co_change.is_empty(),
            "a commit above the path ceiling must emit no pairs"
        );
    }

    #[test]
    fn the_commit_limit_is_disclosed_rather_than_silently_applied() {
        let directory = repository(&[
            &[("a.rs", "1")],
            &[("a.rs", "2")],
            &[("a.rs", "3")],
            &[("a.rs", "4")],
        ]);
        let limits = HistoryLimits {
            max_commits: 2,
            ..HistoryLimits::default()
        };
        let signals = signals(directory.path(), limits);
        let history = signals.history().expect("history");
        assert_eq!(history.commits_walked, 2);
        assert_eq!(history.truncated, Some(Truncation::CommitLimit));
        assert_eq!(
            signals.churn("a.rs"),
            Some(2),
            "the count is a floor over the walked window, and truncated says so"
        );
    }

    #[test]
    fn head_changes_are_recorded_for_the_diff_teleport() {
        let directory = repository(&[&[("a.rs", "1"), ("b.rs", "1")], &[("b.rs", "2")]]);
        let signals = signals(directory.path(), HistoryLimits::default());
        let history = signals.history().expect("history");
        assert_eq!(history.changed_in_head, vec!["b.rs".to_owned()]);
    }

    #[test]
    fn seeding_on_paths_with_no_symbols_falls_back_to_uniform() {
        // Seeding an empty set puts all the mass on nothing, which yields a rank vector unrelated
        // to the request rather than an empty one.
        let ingested = crate::ingest::ingest(
            vec![crate::ingest::SourceInput {
                path: "src/a.rs".to_owned(),
                source: "fn alpha() {}".to_owned(),
            }],
            crate::ingest::IngestLimits::default(),
        );
        let teleport = teleport_for_paths(&ingested.facts, &["docs/readme.md".to_owned()]);
        assert!(matches!(teleport, Teleport::Uniform));

        let teleport = teleport_for_paths(&ingested.facts, &["src/a.rs".to_owned()]);
        assert!(matches!(teleport, Teleport::Seeded(seeds) if seeds.len() == 1));
    }
}
