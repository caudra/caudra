//! Renders one commit as the rows a read-only tab shows.
//!
//! The graph row has about thirty columns to say what a commit was for, which
//! is enough for a subject and nothing else. This is where the rest of the
//! message goes: the editor already scrolls, searches and selects a single
//! column of text, so a commit is worth opening into a tab for the same reason
//! a diff is.
//!
//! The body is laid out verbatim. Commit messages are conventionally wrapped
//! before they are written, and the ones that are not may hold code fences,
//! tables or trailers that re-wrapping would wreck, so long lines are left to
//! the workbench-wide wrap toggle.

use jiff::{Timestamp, tz::TimeZone};

use super::diff::Diff;
use super::repo::{Commit, CommitFiles};
use super::{CUT_SHORT, EMPTY_COMMIT};
use crate::editor::DiffKind;

/// Heads both the first row and the tab the rows are shown in, so the title and
/// the document agree on what is open.
pub const TITLE: &str = "commit ";
/// The header labels are padded to one width so their values line up.
const AUTHOR_LABEL: &str = "Author: ";
const DATE_LABEL: &str = "Date:   ";
const PARENT_LABEL: &str = "Parent: ";
const PARENT_GAP: &str = " ";
const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S %:z";
const UNKNOWN_DATE: &str = "unknown";
/// What the file section says when the walk that would have filled it has not
/// run. A workspace session fetches the paths asynchronously, so a commit can
/// be opened before they arrive.
const FILES_PENDING: &str = "files not listed yet";
const ONE_FILE: &str = "1 file changed";

pub fn detail(commit: &Commit, files: Option<&CommitFiles>) -> Diff {
    let mut diff = Diff::default();
    diff.push(format!("{TITLE}{}", commit.id), DiffKind::Header);
    diff.push(
        format!("{AUTHOR_LABEL}{} <{}>", commit.author, commit.email),
        DiffKind::Header,
    );
    diff.push(
        format!("{DATE_LABEL}{}", date(commit.committed)),
        DiffKind::Header,
    );
    if !commit.parents.is_empty() {
        diff.push(
            format!("{PARENT_LABEL}{}", commit.parents.join(PARENT_GAP)),
            DiffKind::Header,
        );
    }

    diff.push(String::new(), DiffKind::Context);
    diff.push(commit.summary.clone(), DiffKind::Context);
    if let Some(body) = &commit.body {
        diff.push(String::new(), DiffKind::Context);
        for line in body.lines() {
            diff.push(line.to_owned(), DiffKind::Context);
        }
    }

    diff.push(String::new(), DiffKind::Context);
    match files {
        Some(files) => push_files(&mut diff, files),
        None => diff.push(FILES_PENDING.to_owned(), DiffKind::Header),
    }
    diff
}

fn push_files(diff: &mut Diff, files: &CommitFiles) {
    if files.files.is_empty() {
        diff.push(EMPTY_COMMIT.to_owned(), DiffKind::Header);
        return;
    }
    let changed = match files.files.len() {
        1 => ONE_FILE.to_owned(),
        count => format!("{count} files changed"),
    };
    diff.push(changed, DiffKind::Header);
    for file in &files.files {
        diff.push(
            format!("{} {}", file.mark.letter(), file.relative),
            DiffKind::Context,
        );
    }
    if files.truncated {
        diff.push(CUT_SHORT.to_owned(), DiffKind::Header);
    }
}

/// The commit's own time, in the reader's zone. A timestamp git will accept but
/// `jiff` cannot represent is worth saying nothing about rather than refusing to
/// draw the commit it belongs to.
fn date(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .map(|stamp| {
            stamp
                .to_zoned(TimeZone::system())
                .strftime(DATE_FORMAT)
                .to_string()
        })
        .unwrap_or_else(|_| UNKNOWN_DATE.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        AUTHOR_LABEL, CUT_SHORT, Commit, CommitFiles, EMPTY_COMMIT, FILES_PENDING, ONE_FILE,
        PARENT_GAP, PARENT_LABEL, TITLE, detail,
    };
    use crate::editor::DiffKind;
    use crate::fs::tree::GitMark;
    use crate::scm::repo::CommitPath;

    const SUBJECT: &str = "rewrite the scheduler";
    const BODY: &str = "The old one woke every tick.\n\nSigned-off-by: A <a@example.com>";
    const ID: &str = "abc1234";
    const PARENT: &str = "def5678";
    const OTHER_PARENT: &str = "0123456";
    const AUTHOR: &str = "Ada";
    const EMAIL: &str = "ada@example.com";
    const FILE: &str = "src/main.rs";
    const MISSING_LINE: &str = "the detail does not say what the commit says";
    const STRAY_LINE: &str = "the detail says something the commit does not";

    fn commit(body: Option<&str>, parents: &[&str]) -> Commit {
        Commit {
            id: ID.to_owned(),
            summary: SUBJECT.to_owned(),
            body: body.map(str::to_owned),
            author: AUTHOR.to_owned(),
            email: EMAIL.to_owned(),
            committed: 1_700_000_000,
            parents: parents.iter().map(|parent| (*parent).to_owned()).collect(),
        }
    }

    fn files(count: usize, truncated: bool) -> CommitFiles {
        CommitFiles {
            files: (0..count)
                .map(|index| CommitPath {
                    relative: format!("{FILE}{index}"),
                    mark: GitMark::Modified,
                })
                .collect(),
            truncated,
        }
    }

    #[test]
    fn the_detail_names_the_commit_and_lays_its_message_out_whole() {
        let rendered = detail(&commit(Some(BODY), &[PARENT]), Some(&files(1, false)));
        let lines = rendered.lines();

        assert!(lines.contains(&format!("{TITLE}{ID}")), "{MISSING_LINE}");
        assert!(
            lines.contains(&format!("{AUTHOR_LABEL}{AUTHOR} <{EMAIL}>")),
            "{MISSING_LINE}"
        );
        assert!(lines.contains(&SUBJECT.to_owned()), "{MISSING_LINE}");
        for line in BODY.lines() {
            assert!(lines.contains(&line.to_owned()), "{MISSING_LINE}");
        }
    }

    #[test]
    fn a_subject_only_commit_renders_no_body() {
        let rendered = detail(&commit(None, &[PARENT]), Some(&files(1, false)));

        assert_eq!(
            rendered
                .lines()
                .iter()
                .filter(|line| line.as_str() == SUBJECT)
                .count(),
            1,
            "{STRAY_LINE}"
        );
    }

    #[test]
    fn a_merge_names_both_parents_and_a_root_commit_names_none() {
        let merge = detail(&commit(None, &[PARENT, OTHER_PARENT]), None);
        assert!(
            merge
                .lines()
                .contains(&format!("{PARENT_LABEL}{PARENT}{PARENT_GAP}{OTHER_PARENT}")),
            "{MISSING_LINE}"
        );

        let root = detail(&commit(None, &[]), None);
        assert!(
            !root
                .lines()
                .iter()
                .any(|line| line.starts_with(PARENT_LABEL)),
            "{STRAY_LINE}"
        );
    }

    #[test]
    fn the_file_list_carries_its_marks_and_says_when_it_was_cut_short() {
        let rendered = detail(&commit(None, &[PARENT]), Some(&files(2, true)));
        let lines = rendered.lines();

        assert!(
            lines.contains(&"2 files changed".to_owned()),
            "{MISSING_LINE}"
        );
        assert!(
            lines.contains(&format!("{} {FILE}0", GitMark::Modified.letter())),
            "{MISSING_LINE}"
        );
        assert!(lines.contains(&CUT_SHORT.to_owned()), "{MISSING_LINE}");
    }

    #[test]
    fn a_commit_that_touched_one_path_counts_it_in_the_singular() {
        let rendered = detail(&commit(None, &[PARENT]), Some(&files(1, false)));

        assert!(
            rendered.lines().contains(&ONE_FILE.to_owned()),
            "{MISSING_LINE}"
        );
    }

    #[test]
    fn a_commit_that_touched_nothing_says_so_rather_than_listing_nothing() {
        let rendered = detail(&commit(None, &[PARENT]), Some(&files(0, false)));

        assert!(
            rendered.lines().contains(&EMPTY_COMMIT.to_owned()),
            "{MISSING_LINE}"
        );
    }

    #[test]
    fn a_commit_whose_paths_have_not_arrived_says_so() {
        let rendered = detail(&commit(None, &[PARENT]), None);

        assert!(
            rendered.lines().contains(&FILES_PENDING.to_owned()),
            "{MISSING_LINE}"
        );
    }

    #[test]
    fn the_header_block_is_marked_apart_from_the_message() {
        let rendered = detail(&commit(Some(BODY), &[PARENT]), Some(&files(1, false)));
        let subject = rendered
            .rows
            .iter()
            .find(|row| row.text == SUBJECT)
            .expect("the subject row");

        assert_eq!(subject.kind, DiffKind::Context, "{STRAY_LINE}");
        assert_eq!(rendered.rows[0].kind, DiffKind::Header, "{STRAY_LINE}");
    }
}
