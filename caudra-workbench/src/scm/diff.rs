//! Renders the two sides of a change as the lines a diff tab shows.
//!
//! The result is deliberately a unified diff rather than a side-by-side one:
//! the editor already knows how to scroll, search and select a single column of
//! text, and a diff tab is worth having only if it inherits all of that.

use similar::{ChangeTag, TextDiff};

use crate::editor::DiffKind;
use crate::scm::repo::CommitDetail;

const CONTEXT_RADIUS: usize = 3;
const IDENTICAL: &str = "@@ no changes @@";
const ADDED: char = '+';
const REMOVED: char = '-';
const CONTEXT: char = ' ';
const EMPTY_COMMIT: &str = "@@ no file changes @@";
const CUT_SHORT: &str = "@@ more files changed than this tab shows @@";

/// A rendered diff carrying one [`DiffKind`] per line, so the view paints a row
/// without parsing its prefix back out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub lines: Vec<String>,
    pub kinds: Vec<DiffKind>,
}

impl Diff {
    fn push(&mut self, line: String, kind: DiffKind) {
        self.lines.push(line);
        self.kinds.push(kind);
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

pub fn unified(old: &str, new: &str) -> Diff {
    let text = TextDiff::from_lines(old, new);
    let mut diff = Diff::default();
    for group in text.grouped_ops(CONTEXT_RADIUS) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        diff.push(
            format!(
                "@@ -{} +{} @@",
                span(first.old_range().start, last.old_range().end),
                span(first.new_range().start, last.new_range().end)
            ),
            DiffKind::Header,
        );
        for op in &group {
            for change in text.iter_changes(op) {
                let (prefix, kind) = match change.tag() {
                    ChangeTag::Equal => (CONTEXT, DiffKind::Context),
                    ChangeTag::Delete => (REMOVED, DiffKind::Removed),
                    ChangeTag::Insert => (ADDED, DiffKind::Added),
                };
                let value = change.value();
                let body = value.trim_end_matches(['\n', '\r']);
                let mut line = String::with_capacity(body.len() + 1);
                line.push(prefix);
                line.push_str(body);
                diff.push(line, kind);
            }
        }
    }
    if diff.is_empty() {
        diff.push(IDENTICAL.to_owned(), DiffKind::Header);
    }
    diff
}

/// A whole commit as one scrollable column: what the author wrote, then every
/// path they touched, each under the `--- a/… +++ b/…` heading a reader of
/// `git show` already knows how to skim.
pub fn commit(detail: &CommitDetail) -> Diff {
    let mut diff = Diff::default();
    for line in &detail.header {
        diff.push(line.clone(), DiffKind::Header);
    }
    for file in &detail.files {
        diff.push(format!("--- a/{}", file.relative), DiffKind::Removed);
        diff.push(format!("+++ b/{}", file.relative), DiffKind::Added);
        let rendered = unified(&file.old, &file.new);
        diff.lines.extend(rendered.lines);
        diff.kinds.extend(rendered.kinds);
        diff.push(String::new(), DiffKind::Context);
    }
    if detail.files.is_empty() {
        diff.push(EMPTY_COMMIT.to_owned(), DiffKind::Header);
    }
    if detail.truncated {
        diff.push(CUT_SHORT.to_owned(), DiffKind::Header);
    }
    diff
}

/// A hunk header's `start,length` pair. Git counts lines from one, except for
/// an empty range, which it anchors on the line before the gap.
fn span(start: usize, end: usize) -> String {
    let length = end - start;
    let first = if length == 0 { start } else { start + 1 };
    format!("{first},{length}")
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{DiffKind, IDENTICAL, unified};

    const KINDS_MATCH_LINES: &str = "every rendered line must carry a kind";
    const HEADER_FIRST: &str = "a hunk must start with its header";
    const NO_CHANGES: &str = "identical sides must render as a single notice";
    const PREFIX_WRONG: &str = "a line's prefix must agree with its kind";

    fn body(diff: &super::Diff) -> Vec<(&str, DiffKind)> {
        diff.lines
            .iter()
            .map(String::as_str)
            .zip(diff.kinds.iter().copied())
            .collect()
    }

    #[test]
    fn identical_sides_render_a_single_notice() {
        let diff = unified("a\nb\n", "a\nb\n");
        assert_eq!(diff.lines, vec![IDENTICAL.to_owned()], "{NO_CHANGES}");
        assert_eq!(diff.kinds, vec![DiffKind::Header], "{NO_CHANGES}");
    }

    #[test]
    fn a_replaced_line_renders_a_removal_and_an_addition() {
        let diff = unified("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            diff.kinds.first(),
            Some(&DiffKind::Header),
            "{HEADER_FIRST}"
        );
        let rows = body(&diff);
        assert!(rows.contains(&("-b", DiffKind::Removed)), "{PREFIX_WRONG}");
        assert!(rows.contains(&("+B", DiffKind::Added)), "{PREFIX_WRONG}");
        assert!(rows.contains(&(" a", DiffKind::Context)), "{PREFIX_WRONG}");
    }

    #[test]
    fn every_line_carries_a_kind() {
        let diff = unified("one\ntwo\nthree\n", "one\nthree\nfour\n");
        assert_eq!(diff.lines.len(), diff.kinds.len(), "{KINDS_MATCH_LINES}");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let diff = unified("", "hello\nworld\n");
        let kinds: Vec<DiffKind> = diff.kinds.iter().skip(1).copied().collect();
        assert_eq!(
            kinds,
            vec![DiffKind::Added, DiffKind::Added],
            "{PREFIX_WRONG}"
        );
    }

    #[test]
    fn a_deleted_file_is_all_removals() {
        let diff = unified("hello\nworld\n", "");
        let kinds: Vec<DiffKind> = diff.kinds.iter().skip(1).copied().collect();
        assert_eq!(
            kinds,
            vec![DiffKind::Removed, DiffKind::Removed],
            "{PREFIX_WRONG}"
        );
    }

    #[test]
    fn distant_edits_produce_separate_hunks() {
        let old: String = (0..40).map(|line| format!("line {line}\n")).collect();
        let new = old
            .replace("line 1\n", "changed 1\n")
            .replace("line 38\n", "changed 38\n");
        let headers = unified(&old, &new)
            .kinds
            .iter()
            .filter(|kind| **kind == DiffKind::Header)
            .count();
        assert_eq!(headers, 2, "{HEADER_FIRST}");
    }

    #[test_case("a\n", "a\nb\n"; "append")]
    #[test_case("a\nb\n", "a\n"; "truncate")]
    #[test_case("", ""; "both empty")]
    fn a_diff_never_renders_a_bare_newline(old: &str, new: &str) {
        for line in unified(old, new).lines {
            assert!(!line.contains('\n'), "{PREFIX_WRONG}");
        }
    }
}
