//! The repository, through `gix`.
//!
//! Reads are a status walk; writes are index surgery. Staging hashes the
//! worktree file into the object database and points the index entry at it,
//! unstaging points it back at `HEAD`, and both drop the cached tree extension
//! so the next reader rebuilds it rather than trusting a stale one.

use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};

use gix::ObjectId;
use gix::Repository;
use gix::bstr::{BStr, ByteSlice};
use gix::date::time::format::ISO8601;
use gix::index::entry::{Flags, Mode, Stage, Stat};
use gix::object::tree::diff::{Action, Change as TreeChange};
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::tree_index::TrackRenames;

use crate::fs::tree::GitMark;

const LOG_LIMIT: usize = 200;
const SUMMARY_LIMIT: usize = 72;
const ID_LENGTH: usize = 7;
/// How many paths of one commit are diffed. A merge or a formatting sweep can
/// touch thousands, and a tab nobody can scroll to the end of is worse than one
/// that says where it stopped.
const COMMIT_DIFF_FILES: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    #[error("{0} is not inside a git repository")]
    NotARepository(PathBuf),
    #[error("cannot read the repository: {0}")]
    Read(String),
    #[error("cannot update the index: {0}")]
    Write(String),
    #[error("{0} is not tracked and has nothing to restore")]
    Untracked(String),
}

/// One path the repository has something to say about. A path modified both in
/// the index and in the worktree produces two of these, which is what lets the
/// view list it under both headings the way `git status` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub relative: String,
    pub path: PathBuf,
    pub staged: bool,
    pub mark: GitMark,
}

/// One commit of the walk. `parents` carries the same shortened hexadecimal as
/// `id` so the graph can join a commit to its parents by looking no further
/// than the window it is drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub summary: String,
    pub author: String,
    pub parents: Vec<String>,
}

/// What a commit says about itself, and the two sides of every path it touched.
/// Rendering it is [`crate::scm::diff::commit`]'s job: this side only reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommitDetail {
    pub header: Vec<String>,
    pub files: Vec<CommitFile>,
    /// Set when [`COMMIT_DIFF_FILES`] cut the walk short.
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommitFile {
    pub relative: String,
    pub old: String,
    pub new: String,
}

pub struct Repo {
    git_dir: PathBuf,
    workdir: PathBuf,
}

impl Repo {
    /// Walks up from `root` looking for a repository, so opening a workbench
    /// inside a subdirectory still finds it.
    pub fn discover(root: &Path) -> Result<Self, ScmError> {
        let repo = gix::discover(root).map_err(|error| ScmError::Read(error.to_string()))?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| ScmError::NotARepository(root.to_path_buf()))?
            .to_path_buf();
        Ok(Self {
            git_dir: repo.git_dir().to_path_buf(),
            workdir,
        })
    }

    /// A fresh handle for every operation. `gix` answers `index()` from a
    /// snapshot it keeps until the file's modification time moves on, and a
    /// pane that stages and then reads straight back beats that clock: a shared
    /// handle would report the index it staged against.
    fn open(&self) -> Result<Repository, ScmError> {
        gix::open(&self.git_dir).map_err(|error| ScmError::Read(error.to_string()))
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn head_label(&self) -> Option<String> {
        let repo = self.open().ok()?;
        let head = repo.head_ref().ok().flatten();
        match head {
            Some(reference) => Some(reference.name().shorten().to_string()),
            None => repo
                .head_id()
                .ok()
                .map(|id| id.to_hex_with_len(7).to_string()),
        }
    }

    pub fn status(&self) -> Result<Vec<Change>, ScmError> {
        let repo = self.open()?;
        let platform = repo
            .status(gix::progress::Discard)
            .map_err(|error| ScmError::Read(error.to_string()))?
            .index_worktree_rewrites(None)
            .tree_index_track_renames(TrackRenames::Disabled);

        let mut changes = Vec::new();
        for item in platform
            .into_iter(None)
            .map_err(|error| ScmError::Read(error.to_string()))?
        {
            let item = item.map_err(|error| ScmError::Read(error.to_string()))?;
            let relative = item.location().to_str_lossy().into_owned();
            let (staged, mark) = match &item {
                gix::status::Item::TreeIndex(change) => (true, staged_mark(change)),
                gix::status::Item::IndexWorktree(change) => (false, worktree_mark(change)),
            };
            let Some(mark) = mark else {
                continue;
            };
            changes.push(Change {
                path: self.workdir.join(&relative),
                relative,
                staged,
                mark,
            });
        }
        changes.sort_by(|a, b| {
            b.staged
                .cmp(&a.staged)
                .then_with(|| a.relative.cmp(&b.relative))
        });
        Ok(changes)
    }

    pub fn log(&self) -> Result<Vec<Commit>, ScmError> {
        let repo = self.open()?;
        let Ok(head) = repo.head_id() else {
            return Ok(Vec::new());
        };
        let walk = head
            .ancestors()
            .all()
            .map_err(|error| ScmError::Read(error.to_string()))?;

        let mut log = Vec::new();
        for info in walk.take(LOG_LIMIT) {
            let info = info.map_err(|error| ScmError::Read(error.to_string()))?;
            let commit = repo
                .find_commit(info.id)
                .map_err(|error| ScmError::Read(error.to_string()))?;
            let message = commit
                .message()
                .map_err(|error| ScmError::Read(error.to_string()))?;
            log.push(Commit {
                id: short(info.id),
                summary: shorten(&message.summary().to_str_lossy()),
                author: commit
                    .author()
                    .map(|author| author.name.to_str_lossy().into_owned())
                    .unwrap_or_default(),
                parents: commit.parent_ids().map(|id| short(id.detach())).collect(),
            });
        }
        Ok(log)
    }

    /// Everything a commit tab shows: the commit's own words, then the two
    /// sides of each path it changed against its first parent. A root commit is
    /// read against the empty tree, so the first commit of a repository is a
    /// diff rather than an error.
    pub fn commit_detail(&self, id: &str) -> Result<CommitDetail, ScmError> {
        let repo = self.open()?;
        let object = repo
            .rev_parse_single(id)
            .map_err(|error| ScmError::Read(error.to_string()))?;
        let commit = repo
            .find_commit(object)
            .map_err(|error| ScmError::Read(error.to_string()))?;
        let tree = commit
            .tree()
            .map_err(|error| ScmError::Read(error.to_string()))?;
        let parent = match commit.parent_ids().next() {
            Some(parent) => repo
                .find_commit(parent)
                .map_err(|error| ScmError::Read(error.to_string()))?
                .tree()
                .map_err(|error| ScmError::Read(error.to_string()))?,
            None => repo.empty_tree(),
        };

        // Ids are collected first and read afterwards: the walk holds a borrow
        // of the repository, and blobs are cheaper to fetch in a plain loop
        // than inside a callback that has to answer for its own errors.
        let mut touched: Vec<(String, Option<ObjectId>, Option<ObjectId>)> = Vec::new();
        let mut truncated = false;
        parent
            .changes()
            .map_err(|error| ScmError::Read(error.to_string()))?
            .options(|options| {
                options.track_rewrites(None);
            })
            .for_each_to_obtain_tree(&tree, |change| {
                if touched.len() >= COMMIT_DIFF_FILES {
                    truncated = true;
                    return Ok::<Action, Infallible>(Action::Break(()));
                }
                let relative = change.location().to_str_lossy().into_owned();
                let sides = match change {
                    TreeChange::Addition { id, entry_mode, .. } if entry_mode.is_blob() => {
                        (None, Some(id.detach()))
                    }
                    TreeChange::Deletion { id, entry_mode, .. } if entry_mode.is_blob() => {
                        (Some(id.detach()), None)
                    }
                    TreeChange::Modification {
                        previous_id,
                        id,
                        entry_mode,
                        ..
                    } if entry_mode.is_blob() => (Some(previous_id.detach()), Some(id.detach())),
                    _ => return Ok(Action::Continue(())),
                };
                let (old, new) = sides;
                touched.push((relative, old, new));
                Ok(Action::Continue(()))
            })
            .map_err(|error| ScmError::Read(error.to_string()))?;

        let mut files = Vec::with_capacity(touched.len());
        for (relative, old, new) in touched {
            files.push(CommitFile {
                relative,
                old: blob_text(&repo, old)?,
                new: blob_text(&repo, new)?,
            });
        }
        Ok(CommitDetail {
            header: commit_header(&commit, object.detach())?,
            files,
            truncated,
        })
    }

    /// The two sides of a change, as text. `staged` picks `HEAD` against the
    /// index; otherwise it is the index against what is on disk.
    pub fn sides(&self, relative: &str, staged: bool) -> Result<(String, String), ScmError> {
        let repo = self.open()?;
        let path: &BStr = relative.into();
        if staged {
            let old = self.head_blob(&repo, relative)?.unwrap_or_default();
            let new = self.index_blob(&repo, path)?.unwrap_or_default();
            return Ok((old, new));
        }
        let old = self.index_blob(&repo, path)?.unwrap_or_default();
        let new = fs::read_to_string(self.workdir.join(relative)).unwrap_or_default();
        Ok((old, new))
    }

    pub fn stage(&self, relative: &str) -> Result<(), ScmError> {
        let repo = self.open()?;
        let path = self.workdir.join(relative);
        let mut index = self.open_index(&repo)?;

        match fs::symlink_metadata(&path) {
            Err(_) => {
                let name: &BStr = relative.into();
                index.remove_entries(|_, entry, _| entry == name);
            }
            Ok(metadata) => {
                let id = repo
                    .write_blob_stream(
                        fs::File::open(&path)
                            .map_err(|error| ScmError::Write(error.to_string()))?,
                    )
                    .map_err(|error| ScmError::Write(error.to_string()))?
                    .detach();
                let stat = gix::index::fs::Metadata::from_path_no_follow(&path)
                    .ok()
                    .and_then(|meta| Stat::from_fs(&meta).ok())
                    .unwrap_or_default();
                let mode = file_mode(&metadata);
                match index.entry_mut_by_path_and_stage(relative.into(), Stage::Unconflicted) {
                    Some(entry) => {
                        entry.id = id;
                        entry.mode = mode;
                        entry.stat = stat;
                    }
                    None => {
                        index.dangerously_push_entry(
                            stat,
                            id,
                            Flags::empty(),
                            mode,
                            relative.into(),
                        );
                        index.sort_entries();
                    }
                }
            }
        }
        write_index(&mut index)
    }

    pub fn unstage(&self, relative: &str) -> Result<(), ScmError> {
        let repo = self.open()?;
        let mut index = self.open_index(&repo)?;
        let head = self.head_entry(&repo, relative)?;

        match head {
            None => {
                let name: &BStr = relative.into();
                index.remove_entries(|_, entry, _| entry == name);
            }
            Some((id, mode)) => {
                let Some(entry) =
                    index.entry_mut_by_path_and_stage(relative.into(), Stage::Unconflicted)
                else {
                    return Err(ScmError::Untracked(relative.to_owned()));
                };
                entry.id = id;
                entry.mode = mode;
                // Zeroed so the next status re-reads the file rather than
                // trusting a stat that described the staged content.
                entry.stat = Stat::default();
            }
        }
        write_index(&mut index)
    }

    /// Puts the indexed content back on disk, which is what makes a discarded
    /// change disappear from both listings at once. An untracked file has
    /// nothing to restore and is removed instead.
    pub fn discard(&self, relative: &str) -> Result<(), ScmError> {
        let repo = self.open()?;
        let path = self.workdir.join(relative);
        match self.index_blob_bytes(&repo, relative.into())? {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| ScmError::Write(error.to_string()))?;
                }
                fs::write(&path, bytes).map_err(|error| ScmError::Write(error.to_string()))
            }
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(ScmError::Write(error.to_string())),
            },
        }
    }

    fn open_index(&self, repo: &Repository) -> Result<gix::index::File, ScmError> {
        let path = repo.index_path();
        let mut index = match path.exists() {
            true => repo
                .open_index()
                .map_err(|error| ScmError::Write(error.to_string()))?,
            false => gix::index::File::from_state(gix::index::State::new(repo.object_hash()), path),
        };
        // The cached tree describes the index as it was; leaving it behind
        // would let the next reader answer from a tree that no longer matches.
        index.remove_tree();
        Ok(index)
    }

    fn head_entry(
        &self,
        repo: &Repository,
        relative: &str,
    ) -> Result<Option<(gix::ObjectId, Mode)>, ScmError> {
        let Ok(head) = repo.head_commit() else {
            return Ok(None);
        };
        let mut tree = head
            .tree()
            .map_err(|error| ScmError::Read(error.to_string()))?;
        let Some(entry) = tree
            .peel_to_entry_by_path(Path::new(relative))
            .map_err(|error| ScmError::Read(error.to_string()))?
        else {
            return Ok(None);
        };
        let mode = match entry.mode().kind() {
            gix::object::tree::EntryKind::BlobExecutable => Mode::FILE_EXECUTABLE,
            gix::object::tree::EntryKind::Link => Mode::SYMLINK,
            _ => Mode::FILE,
        };
        Ok(Some((entry.id().detach(), mode)))
    }

    fn head_blob(&self, repo: &Repository, relative: &str) -> Result<Option<String>, ScmError> {
        let Some((id, _)) = self.head_entry(repo, relative)? else {
            return Ok(None);
        };
        Ok(Some(text_of(repo, id)?))
    }

    fn index_blob(&self, repo: &Repository, path: &BStr) -> Result<Option<String>, ScmError> {
        let Some(bytes) = self.index_blob_bytes(repo, path)? else {
            return Ok(None);
        };
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn index_blob_bytes(
        &self,
        repo: &Repository,
        path: &BStr,
    ) -> Result<Option<Vec<u8>>, ScmError> {
        let index = repo
            .index_or_empty()
            .map_err(|error| ScmError::Read(error.to_string()))?;
        let Some(entry) = index.entry_by_path_and_stage(path, Stage::Unconflicted) else {
            return Ok(None);
        };
        let object = repo
            .find_object(entry.id)
            .map_err(|error| ScmError::Read(error.to_string()))?;
        Ok(Some(object.data.clone()))
    }
}

/// The commit's own words, ahead of its diff. Read as `DiffKind::Header`, so
/// what the author wrote is told apart from what they changed.
fn commit_header(commit: &gix::Commit<'_>, id: ObjectId) -> Result<Vec<String>, ScmError> {
    let author = commit
        .author()
        .map_err(|error| ScmError::Read(error.to_string()))?;
    let when = author
        .time()
        .map(|time| time.format_or_unix(ISO8601))
        .unwrap_or_default();
    let message = commit
        .message()
        .map_err(|error| ScmError::Read(error.to_string()))?;
    let mut header = vec![
        format!("commit {id}"),
        format!(
            "Author: {} <{}>",
            author.name.to_str_lossy(),
            author.email.to_str_lossy()
        ),
        format!("Date:   {when}"),
        String::new(),
    ];
    let summary = message.summary().to_str_lossy().into_owned();
    let body = message.body.map(|body| body.to_str_lossy().into_owned());
    header.push(format!("    {summary}"));
    if let Some(body) = body {
        header.push(String::new());
        header.extend(body.lines().map(|line| format!("    {line}")));
    }
    header.push(String::new());
    Ok(header)
}

/// A side of a change, which is empty where the path did not exist yet or does
/// not exist any more.
fn blob_text(repo: &Repository, id: Option<ObjectId>) -> Result<String, ScmError> {
    match id {
        Some(id) => text_of(repo, id),
        None => Ok(String::new()),
    }
}

fn short(id: ObjectId) -> String {
    id.to_hex_with_len(ID_LENGTH).to_string()
}

fn text_of(repo: &Repository, id: gix::ObjectId) -> Result<String, ScmError> {
    let object = repo
        .find_object(id)
        .map_err(|error| ScmError::Read(error.to_string()))?;
    Ok(String::from_utf8_lossy(&object.data).into_owned())
}

fn write_index(index: &mut gix::index::File) -> Result<(), ScmError> {
    index
        .write(gix::index::write::Options::default())
        .map_err(|error| ScmError::Write(error.to_string()))
}

fn file_mode(metadata: &fs::Metadata) -> Mode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.file_type().is_symlink() {
            return Mode::SYMLINK;
        }
        if metadata.permissions().mode() & 0o111 != 0 {
            return Mode::FILE_EXECUTABLE;
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Mode::FILE
}

fn staged_mark(change: &gix::diff::index::Change) -> Option<GitMark> {
    Some(match change {
        gix::diff::index::Change::Addition { .. } => GitMark::Added,
        gix::diff::index::Change::Deletion { .. } => GitMark::Deleted,
        gix::diff::index::Change::Modification { .. }
        | gix::diff::index::Change::Rewrite { .. } => GitMark::Modified,
    })
}

fn worktree_mark(item: &WorktreeItem) -> Option<GitMark> {
    use gix::status::index_worktree::iter::Summary;

    Some(match item.summary()? {
        Summary::Removed => GitMark::Deleted,
        Summary::Added | Summary::Copied => GitMark::Untracked,
        Summary::Conflict => GitMark::Conflicted,
        Summary::Modified | Summary::TypeChange | Summary::Renamed | Summary::IntentToAdd => {
            GitMark::Modified
        }
    })
}

fn shorten(summary: &str) -> String {
    match summary.chars().count() > SUMMARY_LIMIT {
        true => summary.chars().take(SUMMARY_LIMIT - 1).collect::<String>() + "\u{2026}",
        false => summary.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{GitMark, ID_LENGTH, Repo};
    use gix::bstr::BString;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    const NOT_LISTED: &str = "the change the test made is not in the status";
    const WRONG_SIDE: &str = "the change is filed under the wrong heading";
    const TEST_AUTHOR: &str = "Workbench Test";
    const TEST_EMAIL: &str = "test@example.invalid";
    const TEST_TIME: &str = "1700000000 +0000";
    const NO_PARENT: &str = "the commit does not name the parents it was built on";
    const NO_HEADER: &str = "the commit tab does not say who wrote it or what they wrote";

    /// A repository with one committed file, so both `HEAD` and the index have
    /// something to say.
    fn repository() -> (TempDir, Repo) {
        let tmp = TempDir::new().unwrap();
        gix::init(tmp.path()).unwrap();
        fs::write(tmp.path().join("tracked.txt"), "one\ntwo\n").unwrap();

        let workbench = Repo::discover(tmp.path()).unwrap();
        workbench.stage("tracked.txt").unwrap();
        commit(tmp.path(), "initial");
        (tmp, workbench)
    }

    /// Writes the index out as a tree and commits it. The test repositories are
    /// flat, so a single level of entries is all this has to cover. The handle
    /// is opened here rather than passed in, for the reason [`Repo::open`]
    /// gives: a handle that staged before the index file's mtime moved on
    /// answers `index()` from the snapshot it took beforehand.
    fn commit(workdir: &Path, message: &str) {
        let repo = gix::open(workdir).unwrap();
        let index = repo.index_or_empty().unwrap();
        let mut tree = gix::objs::Tree::empty();
        for entry in index.entries() {
            tree.entries.push(gix::objs::tree::Entry {
                mode: entry.mode.to_tree_entry_mode().unwrap(),
                filename: entry.path(&index).to_owned(),
                oid: entry.id,
            });
        }
        tree.entries.sort();
        let id = repo.write_object(&tree).unwrap();
        // The test environment has no git identity, and inheriting the
        // developer's would make the log assertions depend on it.
        let who = gix::actor::SignatureRef {
            name: TEST_AUTHOR.into(),
            email: TEST_EMAIL.into(),
            time: TEST_TIME,
        };
        repo.commit_as(who, who, "HEAD", message, id, repo.head_id().ok())
            .unwrap();
    }

    fn marks(repo: &Repo) -> Vec<(String, bool, GitMark)> {
        repo.status()
            .unwrap()
            .into_iter()
            .map(|change| (change.relative, change.staged, change.mark))
            .collect()
    }

    #[test]
    fn a_clean_repository_reports_nothing() {
        let (_tmp, repo) = repository();
        assert!(marks(&repo).is_empty(), "{NOT_LISTED}");
    }

    #[test]
    fn an_untracked_file_is_listed_unstaged_then_staged() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("new.txt"), "hello\n").unwrap();

        assert_eq!(
            marks(&repo),
            vec![("new.txt".to_owned(), false, GitMark::Untracked)],
            "{NOT_LISTED}"
        );

        repo.stage("new.txt").unwrap();

        assert_eq!(
            marks(&repo),
            vec![("new.txt".to_owned(), true, GitMark::Added)],
            "{WRONG_SIDE}"
        );
    }

    #[test]
    fn unstaging_puts_a_modification_back_on_the_worktree_side() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("tracked.txt"), "one\nchanged\n").unwrap();
        repo.stage("tracked.txt").unwrap();
        assert!(marks(&repo)[0].1, "{WRONG_SIDE}");

        repo.unstage("tracked.txt").unwrap();

        assert_eq!(
            marks(&repo),
            vec![("tracked.txt".to_owned(), false, GitMark::Modified)],
            "{WRONG_SIDE}"
        );
    }

    #[test]
    fn unstaging_a_new_file_leaves_it_untracked() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("new.txt"), "hello\n").unwrap();
        repo.stage("new.txt").unwrap();

        repo.unstage("new.txt").unwrap();

        assert_eq!(
            marks(&repo),
            vec![("new.txt".to_owned(), false, GitMark::Untracked)],
            "{WRONG_SIDE}"
        );
    }

    #[test]
    fn discarding_restores_the_indexed_content() {
        let (tmp, repo) = repository();
        let path = tmp.path().join("tracked.txt");
        fs::write(&path, "ruined\n").unwrap();

        repo.discard("tracked.txt").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        assert!(marks(&repo).is_empty(), "{NOT_LISTED}");
    }

    #[test]
    fn discarding_an_untracked_file_removes_it() {
        let (tmp, repo) = repository();
        let path = tmp.path().join("new.txt");
        fs::write(&path, "hello\n").unwrap();

        repo.discard("new.txt").unwrap();

        assert!(!path.exists(), "{NOT_LISTED}");
    }

    #[test]
    fn staging_a_deleted_file_records_the_deletion() {
        let (tmp, repo) = repository();
        fs::remove_file(tmp.path().join("tracked.txt")).unwrap();

        repo.stage("tracked.txt").unwrap();

        assert_eq!(
            marks(&repo),
            vec![("tracked.txt".to_owned(), true, GitMark::Deleted)],
            "{WRONG_SIDE}"
        );
    }

    #[test]
    fn the_two_sides_of_a_worktree_change_are_the_index_and_the_disk() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("tracked.txt"), "one\nthree\n").unwrap();

        let (old, new) = repo.sides("tracked.txt", false).unwrap();

        assert_eq!(old, "one\ntwo\n");
        assert_eq!(new, "one\nthree\n");
    }

    #[test]
    fn the_two_sides_of_a_staged_change_are_head_and_the_index() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("tracked.txt"), "one\nthree\n").unwrap();
        repo.stage("tracked.txt").unwrap();

        let (old, new) = repo.sides("tracked.txt", true).unwrap();

        assert_eq!(old, "one\ntwo\n");
        assert_eq!(new, "one\nthree\n");
    }

    #[test]
    fn the_log_reads_back_the_commits_that_were_made() {
        let (_tmp, repo) = repository();
        let log = repo.log().unwrap();

        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "initial");
        assert_eq!(log[0].id.len(), ID_LENGTH);
    }

    #[test]
    fn a_root_commit_names_its_parents_as_none() {
        let (_tmp, repo) = repository();
        assert!(repo.log().unwrap()[0].parents.is_empty(), "{NO_PARENT}");
    }

    #[test]
    fn a_later_commit_names_the_one_before_it() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("tracked.txt"), "one\nthree\n").unwrap();
        repo.stage("tracked.txt").unwrap();
        commit(tmp.path(), "second");

        let log = repo.log().unwrap();

        assert_eq!(log.len(), 2, "{NOT_LISTED}");
        assert_eq!(log[0].parents, vec![log[1].id.clone()], "{NO_PARENT}");
    }

    #[test]
    fn a_root_commit_is_read_against_the_empty_tree() {
        let (_tmp, repo) = repository();
        let id = repo.log().unwrap()[0].id.clone();

        let detail = repo.commit_detail(&id).unwrap();

        assert_eq!(detail.files.len(), 1, "{NOT_LISTED}");
        assert_eq!(detail.files[0].relative, "tracked.txt");
        assert!(detail.files[0].old.is_empty(), "{WRONG_SIDE}");
        assert_eq!(detail.files[0].new, "one\ntwo\n");
        assert!(!detail.truncated, "{NOT_LISTED}");
    }

    #[test]
    fn a_commit_is_read_against_its_first_parent() {
        let (tmp, repo) = repository();
        fs::write(tmp.path().join("tracked.txt"), "one\nthree\n").unwrap();
        fs::write(tmp.path().join("added.txt"), "fresh\n").unwrap();
        repo.stage("tracked.txt").unwrap();
        repo.stage("added.txt").unwrap();
        commit(tmp.path(), "second");

        let detail = repo.commit_detail(&repo.log().unwrap()[0].id).unwrap();

        let touched: Vec<&str> = detail
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect();
        assert_eq!(touched, vec!["added.txt", "tracked.txt"], "{NOT_LISTED}");
        assert_eq!(detail.files[1].old, "one\ntwo\n", "{WRONG_SIDE}");
        assert_eq!(detail.files[1].new, "one\nthree\n", "{WRONG_SIDE}");
    }

    #[test]
    fn a_commit_header_carries_the_author_and_the_message() {
        let (_tmp, repo) = repository();
        let header = repo
            .commit_detail(&repo.log().unwrap()[0].id)
            .unwrap()
            .header
            .join("\n");

        assert!(header.starts_with("commit "), "{NO_HEADER}");
        assert!(header.contains(TEST_AUTHOR), "{NO_HEADER}");
        assert!(header.contains("    initial"), "{NO_HEADER}");
    }

    #[test]
    fn a_directory_outside_any_repository_is_refused() {
        let tmp = TempDir::new().unwrap();
        assert!(Repo::discover(tmp.path()).is_err());
    }

    #[test]
    fn the_head_label_names_the_branch() {
        let (_tmp, repo) = repository();
        let label = repo.head_label().expect("a branch");
        assert!(!label.is_empty(), "{NOT_LISTED}");
        assert_eq!(BString::from(label.as_str()).to_string(), label);
    }

    #[test]
    fn the_workdir_is_the_root_of_the_repository() {
        let (tmp, repo) = repository();
        assert_eq!(
            repo.workdir().canonicalize().unwrap(),
            Path::new(tmp.path()).canonicalize().unwrap()
        );
    }
}
