//! The repository, through `gix`.
//!
//! Reads are a status walk; writes are index surgery. Staging hashes the
//! worktree file into the object database and points the index entry at it,
//! unstaging points it back at `HEAD`, and both drop the cached tree extension
//! so the next reader rebuilds it rather than trusting a stale one.

use std::fs;
use std::path::{Path, PathBuf};

use gix::Repository;
use gix::bstr::{BStr, ByteSlice};
use gix::index::entry::{Flags, Mode, Stage, Stat};
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::tree_index::TrackRenames;

use crate::fs::tree::GitMark;

const LOG_LIMIT: usize = 200;
const SUMMARY_LIMIT: usize = 72;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub summary: String,
    pub author: String,
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
                id: info.id.to_hex_with_len(7).to_string(),
                summary: shorten(&message.summary().to_str_lossy()),
                author: commit
                    .author()
                    .map(|author| author.name.to_str_lossy().into_owned())
                    .unwrap_or_default(),
            });
        }
        Ok(log)
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
    use super::{GitMark, Repo};
    use gix::bstr::BString;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    const NOT_LISTED: &str = "the change the test made is not in the status";
    const WRONG_SIDE: &str = "the change is filed under the wrong heading";
    const TEST_AUTHOR: &str = "Workbench Test";
    const TEST_EMAIL: &str = "test@example.invalid";
    const TEST_TIME: &str = "1700000000 +0000";

    /// A repository with one committed file, so both `HEAD` and the index have
    /// something to say.
    fn repository() -> (TempDir, Repo) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        fs::write(tmp.path().join("tracked.txt"), "one\ntwo\n").unwrap();

        let workbench = Repo::discover(tmp.path()).unwrap();
        workbench.stage("tracked.txt").unwrap();
        commit(&repo, "initial");
        (tmp, workbench)
    }

    /// Writes the index out as a tree and commits it. The test repositories are
    /// flat, so a single level of entries is all this has to cover.
    fn commit(repo: &gix::Repository, message: &str) {
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
        assert_eq!(log[0].id.len(), 7);
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
