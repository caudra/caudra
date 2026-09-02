//! Single source of truth for turning `(before, after)` file snapshots into
//! structured hunks and unified text. `ToolOutput::Diff` only stores the two
//! snapshots; display text, hunks, and syntax-aware rendering are all derived
//! from them, so they cannot drift out of sync with what was written to disk.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

const CONTEXT_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffSpan {
    pub text: String,
    pub emphasized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiffLine {
    Unchanged(String),
    Added(Vec<DiffSpan>),
    Removed(Vec<DiffSpan>),
}

/// A contiguous group of changes plus surrounding context. `before_start` and
/// `after_start` are 1-indexed and let the renderer position two highlighters
/// (one walking `before`, one walking `after`) so removed lines are highlighted
/// in the old file's parser state and added lines in the new file's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub before_start: usize,
    pub after_start: usize,
    pub lines: Vec<DiffLine>,
}

pub fn compute_hunks(before: &str, after: &str) -> Vec<DiffHunk> {
    let diff = TextDiff::from_lines(before, after);
    let mut hunks = Vec::new();

    for group in diff.grouped_ops(CONTEXT_LINES) {
        let Some(first) = group.first() else { continue };
        let before_start = first.old_range().start + 1;
        let after_start = first.new_range().start + 1;
        let mut lines = Vec::new();

        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let spans: Vec<DiffSpan> = change
                    .iter_strings_lossy()
                    .map(|(emphasized, text)| DiffSpan {
                        text: text.trim_end_matches('\n').to_owned(),
                        emphasized,
                    })
                    .collect();
                lines.push(match change.tag() {
                    ChangeTag::Equal => {
                        DiffLine::Unchanged(spans.into_iter().map(|s| s.text).collect())
                    }
                    ChangeTag::Delete => DiffLine::Removed(spans),
                    ChangeTag::Insert => DiffLine::Added(spans),
                });
            }
        }

        hunks.push(DiffHunk {
            before_start,
            after_start,
            lines,
        });
    }

    hunks
}

/// Added and removed line counts. Walks the changes directly rather than
/// `compute_hunks`, which would build context lines only to discard them.
pub fn stat(before: &str, after: &str) -> String {
    let (added, removed) = TextDiff::from_lines(before, after)
        .iter_all_changes()
        .fold((0, 0), |(added, removed), change| match change.tag() {
            ChangeTag::Insert => (added + 1, removed),
            ChangeTag::Delete => (added, removed + 1),
            ChangeTag::Equal => (added, removed),
        });
    format_stat(added, removed)
}

/// The same summary for an edit that only ever produced a patch, so both
/// shapes of edit report their size identically.
pub fn stat_of_patch(patch: &str) -> String {
    let (added, removed) = patch
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .fold((0, 0), |(added, removed), line| {
            match line.as_bytes().first() {
                Some(b'+') => (added + 1, removed),
                Some(b'-') => (added, removed + 1),
                _ => (added, removed),
            }
        });
    format_stat(added, removed)
}

pub fn format_stat(added: usize, removed: usize) -> String {
    format!("+{added} -{removed}")
}

pub fn unified_text(before: &str, after: &str, summary: &str, display_path: &str) -> String {
    let mut out = format!("{summary}\n--- {display_path}\n+++ {display_path}");
    let write_change = |out: &mut String, prefix: &str, spans: &[DiffSpan]| {
        let _ = write!(out, "\n{prefix}");
        for s in spans {
            out.push_str(&s.text);
        }
    };
    for hunk in compute_hunks(before, after) {
        out.push('\n');
        for dl in &hunk.lines {
            match dl {
                DiffLine::Unchanged(t) => {
                    let _ = write!(out, "\n  {t}");
                }
                DiffLine::Removed(spans) => write_change(&mut out, "- ", spans),
                DiffLine::Added(spans) => write_change(&mut out, "+ ", spans),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLACED_LINE_STAT: &str = "+2 -1";

    /// The two edit shapes reach the stat by different routes, so they are
    /// pinned to the same answer for the same change.
    #[test]
    fn both_edit_shapes_report_the_same_size() {
        let patch = unified_text("a\nb\n", "a\nc\nd\n", "edited", "a.rs");

        assert_eq!(stat("a\nb\n", "a\nc\nd\n"), REPLACED_LINE_STAT);
        assert_eq!(stat_of_patch(&patch), REPLACED_LINE_STAT);
    }

    #[test]
    fn a_patch_header_is_not_counted_as_a_change() {
        let patch = unified_text("a\n", "b\n", "edited", "some/file.rs");

        assert!(
            patch.contains("--- some/file.rs") && patch.contains("+++ some/file.rs"),
            "the header this guards against must be present: {patch}"
        );
        assert_eq!(stat_of_patch(&patch), "+1 -1");
    }

    fn line_text(line: &DiffLine) -> String {
        match line {
            DiffLine::Unchanged(s) => s.clone(),
            DiffLine::Added(spans) | DiffLine::Removed(spans) => {
                spans.iter().map(|s| s.text.as_str()).collect()
            }
        }
    }

    #[test]
    fn hunk_starts_are_one_indexed_and_lines_have_no_trailing_newline() {
        let before = "a\nb\nc\nd\nOLD\nf\ng\nh\ni\n";
        let after = "a\nb\nc\nd\nNEW\nf\ng\nh\ni\n";
        let hunks = compute_hunks(before, after);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].before_start, hunks[0].after_start), (2, 2));
        assert!(hunks[0].lines.iter().all(|l| !line_text(l).contains('\n')));
    }

    #[test]
    fn no_change_returns_empty() {
        let s = "a\nb\nc\n";
        assert!(compute_hunks(s, s).is_empty());
    }

    /// When an earlier insertion shifts subsequent line numbers, a later
    /// hunk's `after_start` must reflect the post-insertion line, not the
    /// original. The renderer relies on this to keep its AFTER walker aligned
    /// with the actual file coordinates. The two changes are placed far apart
    /// so they always land in distinct grouped hunks.
    #[test]
    fn after_start_tracks_post_insertion_line_numbers() {
        let mut before: Vec<String> = (1..=40).map(|i| i.to_string()).collect();
        let mut after = before.clone();
        after.insert(1, "INS".into());
        before[35] = "OLD".into();
        after[36] = "NEW".into();
        let hunks = compute_hunks(&before.join("\n"), &after.join("\n"));
        let last = hunks.last().expect("at least one hunk");
        assert_eq!(last.after_start, last.before_start + 1);
    }

    #[test]
    fn unified_text_renders_summary_header_and_all_line_kinds() {
        let before = "keep\nold\n";
        let after = "keep\nnew\n";
        let text = unified_text(before, after, "Edited foo", "src/main.rs");
        assert!(text.starts_with("Edited foo"));
        assert!(text.contains("--- src/main.rs"));
        assert!(text.contains("+++ src/main.rs"));
        assert!(text.contains("  keep"));
        assert!(text.contains("- old"));
        assert!(text.contains("+ new"));
    }
}
