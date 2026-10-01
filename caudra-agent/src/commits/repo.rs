//! Reading commits out of the project's repository, through `gix`.
//!
//! Two questions are asked of a repository and no more: what the recent log
//! holds, which is the window the composer searches and validates against, and
//! what one commit of it says, which is what the model is handed. Nothing here
//! writes, and nothing here reads a blob: a commit mention carries a message
//! and a list of paths, never a diff.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use caudra_workspace::ScmChangeKind;
use gix::bstr::ByteSlice;
use gix::object::tree::diff::{Action, Change as TreeChange};

/// How far back a commit mention can reach. One number governs the local read,
/// the window a remote workspace is asked for, and the search send-time
/// resolution runs, so the popup can only ever offer a commit that resolves.
pub const LOG_WINDOW: usize = 200;
/// Digits of an abbreviated hash, matching what the workbench displays.
pub const ID_LENGTH: usize = 7;
/// Characters of a full hash. A run this long is not plausible prose, which is
/// what lets one resolve before any log has been read.
pub const FULL_ID_LENGTH: usize = 40;
/// Paths one commit may list. A merge or a formatting sweep can touch
/// thousands, and a list nobody will read is not worth the turn's budget.
const COMMIT_FILES: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("{0} is not inside a git repository")]
    NotARepository(PathBuf),
    #[error("no commit matches {0}")]
    Unknown(String),
    #[error("the repository could not be read: {0}")]
    Read(String),
}

/// One commit of the log window, as the popup lists it and the scanner
/// validates against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    /// The full hash. Abbreviations are resolved against this by prefix, so one
    /// index answers for every length the grammar admits.
    pub id: String,
    pub subject: String,
    pub author: String,
    pub committed_unix_seconds: i64,
}

impl CommitSummary {
    pub fn short(&self) -> &str {
        &self.id[..ID_LENGTH.min(self.id.len())]
    }
}

/// Everything a commit mention puts in front of the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub committed_unix_seconds: i64,
    pub subject: String,
    /// The message past its subject line, already trimmed. Empty when the
    /// commit has only a subject, which is not an error.
    pub body: String,
    pub files: Vec<CommitFile>,
    /// Set when [`COMMIT_FILES`] cut the listing short.
    pub files_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    pub path: String,
    pub change: ScmChangeKind,
}

/// The most recent `limit` commits reachable from `HEAD`.
///
/// A repository with no commits yet is empty rather than an error: a fresh
/// project is a normal thing to be typing in.
pub fn log(root: &Path, limit: usize) -> Result<Vec<CommitSummary>, CommitError> {
    let repo = discover(root)?;
    let Ok(head) = repo.head_id() else {
        return Ok(Vec::new());
    };
    let walk = head.ancestors().all().map_err(read)?;

    let mut log = Vec::new();
    for info in walk.take(limit) {
        let info = info.map_err(read)?;
        let commit = repo.find_commit(info.id).map_err(read)?;
        let signature = commit.author().map_err(read)?;
        log.push(CommitSummary {
            id: info.id.to_string(),
            subject: subject_of(&commit)?,
            author: signature.name.to_str_lossy().trim().to_owned(),
            committed_unix_seconds: commit.time().map(|time| time.seconds).unwrap_or_default(),
        });
    }
    Ok(log)
}

/// One commit in full, by full or abbreviated hash.
pub fn show(root: &Path, id: &str) -> Result<CommitDetail, CommitError> {
    let repo = discover(root)?;
    let object = repo
        .rev_parse_single(id)
        .map_err(|_| CommitError::Unknown(id.to_owned()))?;
    let commit = repo
        .find_commit(object)
        .map_err(|_| CommitError::Unknown(id.to_owned()))?;
    let signature = commit.author().map_err(read)?;
    let message = commit.message_raw().map_err(read)?.to_str_lossy();
    let (subject, body) = split_message(&message);
    let (files, files_truncated) = files_of(&repo, &commit)?;

    Ok(CommitDetail {
        id: commit.id().to_string(),
        parents: commit
            .parent_ids()
            .map(|id| id.detach().to_string())
            .collect(),
        author_name: signature.name.to_str_lossy().trim().to_owned(),
        author_email: signature.email.to_str_lossy().trim().to_owned(),
        committed_unix_seconds: commit.time().map(|time| time.seconds).unwrap_or_default(),
        subject,
        body,
        files,
        files_truncated,
    })
}

/// Every path a commit changed against its first parent, and how. A root commit
/// is read against the empty tree, so the first commit of a repository lists its
/// files rather than failing.
fn files_of(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<(Vec<CommitFile>, bool), CommitError> {
    let tree = commit.tree().map_err(read)?;
    let parent = match commit.parent_ids().next() {
        Some(parent) => repo
            .find_commit(parent)
            .map_err(read)?
            .tree()
            .map_err(read)?,
        None => repo.empty_tree(),
    };

    let mut files = Vec::new();
    let mut truncated = false;
    parent
        .changes()
        .map_err(read)?
        .options(|options| {
            options.track_rewrites(None);
        })
        .for_each_to_obtain_tree(&tree, |change| {
            if files.len() >= COMMIT_FILES {
                truncated = true;
                return Ok::<Action, Infallible>(Action::Break(()));
            }
            let kind = match change {
                TreeChange::Addition { entry_mode, .. } if entry_mode.is_blob() => {
                    ScmChangeKind::Added
                }
                TreeChange::Deletion { entry_mode, .. } if entry_mode.is_blob() => {
                    ScmChangeKind::Deleted
                }
                TreeChange::Modification { entry_mode, .. } if entry_mode.is_blob() => {
                    ScmChangeKind::Modified
                }
                _ => return Ok(Action::Continue(())),
            };
            files.push(CommitFile {
                path: change.location().to_str_lossy().into_owned(),
                change: kind,
            });
            Ok(Action::Continue(()))
        })
        .map_err(read)?;

    Ok((files, truncated))
}

/// Walks up from `root`, so a session opened in a subdirectory still finds the
/// repository it is inside.
fn discover(root: &Path) -> Result<gix::Repository, CommitError> {
    gix::discover(root).map_err(|_| CommitError::NotARepository(root.to_path_buf()))
}

fn subject_of(commit: &gix::Commit<'_>) -> Result<String, CommitError> {
    let message = commit.message_raw().map_err(read)?.to_str_lossy();
    Ok(split_message(&message).0)
}

/// Splits a raw commit message into its subject line and the rest.
///
/// The first line alone is the subject, which is what git's own abbreviation
/// does and what the remote workspace reports, so local and remote mentions
/// read the same however the commit was wrapped.
fn split_message(message: &str) -> (String, String) {
    let message = message.trim_end();
    let (subject, body) = message.split_once('\n').unwrap_or((message, ""));
    (subject.trim().to_owned(), body.trim().to_owned())
}

fn read(error: impl std::fmt::Display) -> CommitError {
    CommitError::Read(error.to_string())
}

/// The abbreviation the composer inserts for a full hash.
pub fn abbreviate(id: &str) -> String {
    id.get(..ID_LENGTH).unwrap_or(id).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::ObjectId;
    use gix::actor::SignatureRef;
    use gix::bstr::BString;

    const AUTHOR: &str = "Ada Lovelace";
    const EMAIL: &str = "ada@example.com";
    const AUTHOR_TIME: &str = "1700000000 +0000";
    const COMMITTER_TIME: &str = "1700003600 +0530";
    const COMMITTED_UNIX_SECONDS: i64 = 1_700_003_600;
    const SUBJECT: &str = "Fix login crash on empty session";
    const BODY: &str = "Sessions restored from disk could carry an empty\ntoken list.";
    const EXPECT_FILES: &str = "the commit lists the paths it touched";

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn short(id: ObjectId) -> String {
        id.to_hex_with_len(ID_LENGTH).to_string()
    }

    /// A repository built through `gix` rather than the `git` binary, so the
    /// tests do not depend on what is installed.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("a temporary directory");
        gix::init(dir.path()).expect("a repository");
        Fixture {
            root: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    fn write_blob(repo: &gix::Repository, content: &str) -> ObjectId {
        repo.write_blob(content.as_bytes()).expect("a blob").into()
    }

    fn write_tree(repo: &gix::Repository, entries: &[(&str, ObjectId)]) -> ObjectId {
        let mut tree = gix::objs::Tree::empty();
        for (name, id) in entries {
            tree.entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: BString::from(*name),
                oid: *id,
            });
        }
        tree.entries.sort();
        repo.write_object(&tree).expect("a tree").detach()
    }

    fn commit(
        repo: &gix::Repository,
        message: &str,
        tree: ObjectId,
        parents: Vec<ObjectId>,
    ) -> ObjectId {
        let author = SignatureRef {
            name: AUTHOR.into(),
            email: EMAIL.into(),
            time: AUTHOR_TIME,
        };
        let committer = SignatureRef {
            time: COMMITTER_TIME,
            ..author
        };
        repo.commit_as(committer, author, "HEAD", message, tree, parents)
            .expect("a commit")
            .detach()
    }

    /// One commit adding `first.txt`, then one modifying it and adding
    /// `second.txt`, so both sides of the listing have something to say.
    fn two_commits(fixture: &Fixture) -> (ObjectId, ObjectId) {
        let repo = gix::open(&fixture.root).expect("a repository");
        let blob = write_blob(&repo, "one\n");
        let root_tree = write_tree(&repo, &[("first.txt", blob)]);
        let root = commit(&repo, "initial\n", root_tree, Vec::new());

        let changed = write_blob(&repo, "two\n");
        let added = write_blob(&repo, "new\n");
        let tree = write_tree(&repo, &[("first.txt", changed), ("second.txt", added)]);
        let tip = commit(&repo, &format!("{SUBJECT}\n\n{BODY}\n"), tree, vec![root]);
        (root, tip)
    }

    #[test]
    fn a_repository_with_no_commits_logs_nothing() {
        let fixture = fixture();
        assert!(log(&fixture.root, LOG_WINDOW).expect("a log").is_empty());
    }

    #[test]
    fn a_path_outside_a_repository_is_named_as_such() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(
            log(dir.path(), LOG_WINDOW),
            Err(CommitError::NotARepository(_))
        ));
    }

    #[test]
    fn the_log_reads_newest_first() {
        let fixture = fixture();
        let (root, tip) = two_commits(&fixture);
        let log = log(&fixture.root, LOG_WINDOW).expect("a log");
        assert_eq!(
            log.iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            [tip.to_string(), root.to_string()]
        );
        assert_eq!(log[0].subject, SUBJECT);
        assert_eq!(log[0].author, AUTHOR);
        assert_eq!(log[0].committed_unix_seconds, COMMITTED_UNIX_SECONDS);
    }

    #[test]
    fn a_multi_line_message_keeps_its_body() {
        let fixture = fixture();
        let (_, tip) = two_commits(&fixture);
        let detail = show(&fixture.root, &short(tip)).expect("a commit");
        assert_eq!(detail.subject, SUBJECT);
        assert_eq!(detail.body, BODY);
        assert_eq!(detail.committed_unix_seconds, COMMITTED_UNIX_SECONDS);
    }

    #[test]
    fn a_commit_lists_what_it_changed_against_its_parent() {
        let fixture = fixture();
        let (_, tip) = two_commits(&fixture);
        let detail = show(&fixture.root, &short(tip)).expect("a commit");
        assert_eq!(
            detail
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.change))
                .collect::<Vec<_>>(),
            [
                ("first.txt", ScmChangeKind::Modified),
                ("second.txt", ScmChangeKind::Added),
            ],
            "{EXPECT_FILES}"
        );
        assert!(!detail.files_truncated);
    }

    /// A root commit has no parent to read against, and must still list what it
    /// introduced rather than failing.
    #[test]
    fn a_root_commit_lists_its_files_against_the_empty_tree() {
        let fixture = fixture();
        let (root, _) = two_commits(&fixture);
        let detail = show(&fixture.root, &short(root)).expect("a commit");
        assert_eq!(
            detail
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.change))
                .collect::<Vec<_>>(),
            [("first.txt", ScmChangeKind::Added)],
            "{EXPECT_FILES}"
        );
        assert!(detail.parents.is_empty());
    }

    #[test]
    fn an_abbreviated_hash_resolves_to_the_same_commit_as_the_full_one() {
        let fixture = fixture();
        let (_, tip) = two_commits(&fixture);
        let full = show(&fixture.root, &tip.to_string()).expect("a commit");
        let abbreviated = show(&fixture.root, &short(tip)).expect("a commit");
        assert_eq!(full, abbreviated);
    }

    #[test]
    fn a_hash_no_commit_carries_is_named_as_unknown() {
        let fixture = fixture();
        two_commits(&fixture);
        assert!(matches!(
            show(&fixture.root, "0000000"),
            Err(CommitError::Unknown(_))
        ));
    }

    #[test]
    fn a_subject_only_commit_has_an_empty_body() {
        let fixture = fixture();
        let (root, _) = two_commits(&fixture);
        assert!(
            show(&fixture.root, &short(root))
                .expect("a commit")
                .body
                .is_empty()
        );
    }
}
