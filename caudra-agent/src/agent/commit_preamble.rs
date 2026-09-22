//! Turns `#hash` commit mentions into hidden context ahead of the user's turn.
//!
//! Each reference becomes one synthetic message carrying what the commit says
//! and which paths it touched, so the visible transcript keeps the short
//! `#a1b2c3d` the user typed while the model is handed the commit behind it.
//!
//! Unlike a file, a commit is immutable, so resolving at send time is a matter
//! of where the code lives rather than what it reads: the same hash answers the
//! same way whenever it is asked. What send-time resolution does buy is that a
//! draft, a queued turn and a restored session all store the reference alone,
//! and that headless runs get the same spelling without a composer.
//!
//! No diff is read. A commit mention is a message and a list of paths; a model
//! that wants the change itself can ask for it.

use caudra_providers::Message;
use caudra_workspace::{
    ScmChangeKind, ScmDiffLineKind, ScmDiffRequest, ScmDiffTarget, ScmDiscoverRequest, ScmLogPage,
    ScmLogRequest, ScmRevision, WorkspacePath, WorkspaceSession,
};

use crate::agent::mention_preamble::BUDGET_ERROR;
use crate::commits::CommitRef;
use crate::commits::repo::{self, CommitDetail, CommitError, CommitFile};
use crate::tools::ToolContext;

/// Bytes one commit may spend of the turn's budget. A merge with a thousand
/// paths must not crowd out every other attachment the turn asked for.
const MAX_COMMIT_BYTES: usize = 16 * 1024;
const UNAVAILABLE_ERROR: &str = "not inlined: source control is unavailable for this workspace";
const NO_BODY_NOTE: &str = "this workspace did not report the message body";
/// Lines of the remote tree diff read to recover the path list. Only the file
/// headers are kept, but the service counts every line it walks.
const REMOTE_DIFF_LINES: u32 = 4 * 1024;
const REMOTE_DIFF_BYTES: u32 = 512 * 1024;

pub struct Resolution<'a> {
    pub root: &'a std::path::Path,
    pub context: &'a ToolContext,
}

pub async fn build(
    commits: &[CommitRef],
    resolution: Resolution<'_>,
    budget: &mut usize,
) -> Vec<Message> {
    let mut seen: Vec<&str> = Vec::with_capacity(commits.len());
    let mut messages = Vec::new();
    for commit in commits {
        if seen.contains(&commit.id.as_str()) {
            continue;
        }
        seen.push(&commit.id);
        messages.push(resolve(commit, &resolution, budget).await);
    }
    messages
}

async fn resolve(commit: &CommitRef, resolution: &Resolution<'_>, budget: &mut usize) -> Message {
    if *budget == 0 {
        return note(commit, BUDGET_ERROR);
    }
    let detail = match resolution.context.workspace_session.as_ref() {
        Some(session) => remote_detail(commit, session).await,
        None => repo::show(resolution.root, &commit.id)
            .map(|detail| (detail, None))
            .map_err(describe),
    };
    match detail {
        Ok((detail, missing_body)) => {
            let body = render(&detail, missing_body);
            *budget = budget.saturating_sub(body.len());
            Message::mention(body)
        }
        Err(error) => note(commit, &error),
    }
}

/// The remote workspace answers through its own source-control service rather
/// than a repository this process can open. The log carries the message, and a
/// tree diff against the first parent carries the paths.
async fn remote_detail(
    commit: &CommitRef,
    session: &WorkspaceSession,
) -> Result<(CommitDetail, Option<&'static str>), String> {
    let service = session
        .workspace()
        .services()
        .scm_read
        .as_ref()
        .ok_or_else(|| UNAVAILABLE_ERROR.to_owned())?;
    let repository = service
        .discover(
            session.binding(),
            session.cursor(),
            &ScmDiscoverRequest {
                path: WorkspacePath::root(),
            },
        )
        .await
        .map_err(|_| "not inlined: the remote repository could not be found".to_owned())?
        .repository;

    let page: ScmLogPage = service
        .log(
            session.binding(),
            session.cursor(),
            &ScmLogRequest {
                repository_handle: repository.handle.clone(),
                // The same window the composer listed, so a reference that
                // resolved there resolves here.
                page_size: repo::LOG_WINDOW as u32,
                continuation: None,
            },
        )
        .await
        .map_err(|_| "not inlined: the remote log could not be read".to_owned())?;

    let found = page
        .commits
        .iter()
        .find(|candidate| candidate.id.as_str().starts_with(&commit.id))
        .ok_or_else(|| {
            format!(
                "not inlined: no commit in the remote log matches {}",
                commit.id
            )
        })?;

    let parent = found.parents.first().cloned();
    let files = remote_files(session, &repository.handle, &found.id, parent.as_ref())
        .await
        .unwrap_or_default();

    let (body, missing_body) = match &found.body {
        Some(body) => (body.clone(), None),
        None => (String::new(), Some(NO_BODY_NOTE)),
    };

    let detail = CommitDetail {
        id: found.id.as_str().to_owned(),
        parents: found
            .parents
            .iter()
            .map(|parent| parent.as_str().to_owned())
            .collect(),
        author_name: found.author_name.clone(),
        author_email: found.author_email.clone(),
        committed_unix_seconds: found.committed_unix_seconds,
        subject: found.summary.clone(),
        body,
        files,
        files_truncated: false,
    };
    Ok((detail, missing_body))
}

/// The paths one remote commit touched, read off the file headers of a tree
/// diff. A failure here costs the listing and not the message, so the caller
/// treats it as an empty list rather than an error.
async fn remote_files(
    session: &WorkspaceSession,
    handle: &caudra_workspace::ResourceId,
    target: &ScmRevision,
    parent: Option<&ScmRevision>,
) -> Option<Vec<CommitFile>> {
    let parent = parent?;
    let service = session.workspace().services().scm_read.as_ref()?;
    let page = service
        .diff(
            session.binding(),
            session.cursor(),
            &ScmDiffRequest {
                repository_handle: handle.clone(),
                target: ScmDiffTarget::Tree {
                    base: parent.clone(),
                    target: target.clone(),
                },
                path: None,
                max_lines: REMOTE_DIFF_LINES,
                max_bytes: REMOTE_DIFF_BYTES,
                continuation: None,
            },
        )
        .await
        .ok()?;
    Some(
        page.lines
            .into_iter()
            .filter(|line| line.kind == ScmDiffLineKind::File)
            .map(|line| CommitFile {
                path: line.path.as_str().to_owned(),
                change: line.change.unwrap_or(ScmChangeKind::Modified),
            })
            .collect(),
    )
}

/// The XML one commit contributes, bounded so a single mention cannot spend the
/// whole turn on a path list.
fn render(detail: &CommitDetail, missing_body: Option<&str>) -> String {
    let mut out = String::with_capacity(detail.body.len() + detail.subject.len() + 256);
    out.push_str(&open_tag(detail));
    out.push_str("\n<subject>");
    out.push_str(&escape(&detail.subject));
    out.push_str("</subject>\n");
    match missing_body {
        // A workspace that cannot report the body says so in the element that
        // would have carried it. Silence there reads as "the commit had none".
        Some(reason) => {
            out.push_str("<message unavailable=\"");
            out.push_str(&escape(reason));
            out.push_str("\" />\n");
        }
        None if !detail.body.is_empty() => {
            out.push_str("<message>\n");
            out.push_str(&escape(&detail.body));
            out.push_str("\n</message>\n");
        }
        None => {}
    }
    out.push_str(&files_block(detail));
    out.push_str("</commit>");
    if out.len() > MAX_COMMIT_BYTES {
        out.truncate(floor_char_boundary(&out, MAX_COMMIT_BYTES));
        out.push_str("\n[commit truncated]\n</commit>");
    }
    out
}

fn files_block(detail: &CommitDetail) -> String {
    if detail.files.is_empty() {
        return String::new();
    }
    let mut out = String::from(match detail.files_truncated {
        true => "<files truncated=\"true\">\n",
        false => "<files>\n",
    });
    for file in &detail.files {
        out.push_str(mark(file.change));
        out.push(' ');
        out.push_str(&escape(&file.path));
        out.push('\n');
    }
    out.push_str("</files>\n");
    out
}

fn open_tag(detail: &CommitDetail) -> String {
    let author = match detail.author_email.is_empty() {
        true => detail.author_name.clone(),
        false => format!("{} <{}>", detail.author_name, detail.author_email),
    };
    format!(
        "<commit hash=\"{}\" author=\"{}\" date=\"{}\">",
        escape(&repo::abbreviate(&detail.id)),
        escape(&author),
        timestamp(detail.committed_unix_seconds)
    )
}

/// A reference that produced no commit still reaches the model, so it can tell
/// "no such revision" from "the commit was empty".
fn note(commit: &CommitRef, error: &str) -> Message {
    Message::mention(format!(
        "<commit hash=\"{}\" error=\"{}\" />",
        escape(&commit.id),
        escape(error)
    ))
}

fn describe(error: CommitError) -> String {
    match error {
        CommitError::NotARepository(_) => {
            "not inlined: this project is not inside a git repository".to_owned()
        }
        CommitError::Unknown(id) => format!("not inlined: no commit matches {id}"),
        CommitError::Read(reason) => format!("not inlined: {reason}"),
    }
}

const fn mark(change: ScmChangeKind) -> &'static str {
    match change {
        ScmChangeKind::Added => "A",
        ScmChangeKind::Deleted => "D",
        ScmChangeKind::Modified => "M",
        ScmChangeKind::Renamed => "R",
        ScmChangeKind::Copied => "C",
        ScmChangeKind::TypeChanged => "T",
        ScmChangeKind::Unmerged => "U",
    }
}

fn timestamp(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .map(|stamp| stamp.to_string())
        .unwrap_or_default()
}

/// Attribute and text escaping. The model is handed XML, so a commit message
/// containing a bracket must not be able to close a tag that is still open.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
    out
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::ContentBlock;
    use test_case::test_case;

    const HASH: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const SHORT: &str = "a1b2c3d";
    const SUBJECT: &str = "Fix login crash";
    const BODY: &str = "Sessions could carry an empty token list.";
    const AUTHOR: &str = "Ada Lovelace";
    const EMAIL: &str = "ada@example.com";

    fn text_of(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn detail(body: &str, files: Vec<CommitFile>) -> CommitDetail {
        CommitDetail {
            id: HASH.to_owned(),
            parents: vec!["0f1e2d3".to_owned()],
            author_name: AUTHOR.to_owned(),
            author_email: EMAIL.to_owned(),
            committed_unix_seconds: 1_700_000_000,
            subject: SUBJECT.to_owned(),
            body: body.to_owned(),
            files,
            files_truncated: false,
        }
    }

    fn file(path: &str, change: ScmChangeKind) -> CommitFile {
        CommitFile {
            path: path.to_owned(),
            change,
        }
    }

    #[test]
    fn a_commit_renders_its_metadata_message_and_paths() {
        let rendered = render(
            &detail(
                BODY,
                vec![
                    file("src/auth.rs", ScmChangeKind::Modified),
                    file("src/guard.rs", ScmChangeKind::Added),
                ],
            ),
            None,
        );
        assert!(
            rendered.starts_with(&format!(
                "<commit hash=\"{SHORT}\" author=\"{AUTHOR} &lt;{EMAIL}&gt;\" date=\"2023-11-14T22:13:20Z\">"
            )),
            "{rendered}"
        );
        assert!(rendered.contains(&format!("<subject>{SUBJECT}</subject>")));
        assert!(rendered.contains(&format!("<message>\n{BODY}\n</message>")));
        assert!(rendered.contains("M src/auth.rs\nA src/guard.rs\n"));
        assert!(rendered.ends_with("</commit>"));
    }

    /// The hash is abbreviated for the model the same way it is for the reader,
    /// so what the transcript shows and what the request carries agree.
    #[test]
    fn the_open_tag_abbreviates_the_hash() {
        assert!(open_tag(&detail(BODY, Vec::new())).contains(SHORT));
        assert!(!open_tag(&detail(BODY, Vec::new())).contains(HASH));
    }

    /// A remote workspace that cannot report a body must not look like a commit
    /// whose message was a subject and nothing else.
    #[test]
    fn an_unreported_body_is_announced_rather_than_left_blank() {
        let rendered = render(&detail("", Vec::new()), Some(NO_BODY_NOTE));
        assert!(
            rendered.contains(&format!("<message unavailable=\"{NO_BODY_NOTE}\" />")),
            "{rendered}"
        );
    }

    #[test]
    fn a_commit_without_a_body_omits_the_message_element() {
        let rendered = render(&detail("", Vec::new()), None);
        assert!(!rendered.contains("<message>"), "{rendered}");
        assert!(rendered.contains("<subject>"), "{rendered}");
    }

    #[test]
    fn a_commit_that_touched_nothing_omits_the_files_element() {
        assert!(!render(&detail(BODY, Vec::new()), None).contains("<files"));
    }

    #[test]
    fn a_truncated_listing_says_so_on_the_element() {
        let mut truncated = detail(BODY, vec![file("a.rs", ScmChangeKind::Added)]);
        truncated.files_truncated = true;
        assert!(render(&truncated, None).contains("<files truncated=\"true\">"));
    }

    #[test_case(ScmChangeKind::Added, "A" ; "added")]
    #[test_case(ScmChangeKind::Deleted, "D" ; "deleted")]
    #[test_case(ScmChangeKind::Modified, "M" ; "modified")]
    #[test_case(ScmChangeKind::Renamed, "R" ; "renamed")]
    fn a_change_is_marked_the_way_git_marks_it(change: ScmChangeKind, expected: &str) {
        assert!(
            render(&detail(BODY, vec![file("a.rs", change)]), None)
                .contains(&format!("{expected} a.rs"))
        );
    }

    /// A message is prose a person wrote, so it can contain anything. None of
    /// it may be able to close a tag the renderer left open.
    #[test]
    fn markup_in_a_message_cannot_close_the_element_around_it() {
        let rendered = render(&detail("</commit><injected>", Vec::new()), None);
        assert!(!rendered.contains("<injected>"), "{rendered}");
        assert!(rendered.contains("&lt;injected&gt;"), "{rendered}");
        assert_eq!(rendered.matches("</commit>").count(), 1, "{rendered}");
    }

    #[test]
    fn a_quote_in_an_author_name_cannot_escape_the_attribute() {
        let mut quoted = detail(BODY, Vec::new());
        quoted.author_name = "A \" name".to_owned();
        assert!(open_tag(&quoted).contains("&quot;"));
    }

    #[test]
    fn a_note_is_self_closing_and_names_the_hash() {
        let message = note(&CommitRef::new(SHORT), "no such thing");
        assert_eq!(
            text_of(&message),
            format!("<commit hash=\"{SHORT}\" error=\"no such thing\" />")
        );
    }

    /// A commit large enough to swamp the turn is cut rather than dropped, and
    /// the cut is announced inside a closed element.
    #[test]
    fn an_oversized_commit_is_cut_and_still_closes_its_element() {
        let files = (0..4_000)
            .map(|index| file(&format!("src/file{index}.rs"), ScmChangeKind::Added))
            .collect();
        let rendered = render(&detail(BODY, files), None);
        assert!(rendered.len() < MAX_COMMIT_BYTES + 64, "{}", rendered.len());
        assert!(rendered.ends_with("[commit truncated]\n</commit>"));
    }

    #[test]
    fn a_multibyte_message_is_cut_on_a_character_boundary() {
        let rendered = render(&detail(&"é".repeat(MAX_COMMIT_BYTES), Vec::new()), None);
        assert!(rendered.ends_with("[commit truncated]\n</commit>"));
    }

    #[test]
    fn an_author_without_an_email_is_named_alone() {
        let mut anonymous = detail(BODY, Vec::new());
        anonymous.author_email = String::new();
        assert!(open_tag(&anonymous).contains(&format!("author=\"{AUTHOR}\"")));
    }
}
