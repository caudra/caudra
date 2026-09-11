//! Single source of truth for turning `(before, after)` file snapshots into
//! structured hunks and unified text. `ToolOutput::Diff` only stores the two
//! snapshots; display text, hunks, and syntax-aware rendering are all derived
//! from them, so they cannot drift out of sync with what was written to disk.
//!
//! Lines are aligned on their *trimmed* content, the way VS Code's
//! `DefaultLinesDiffComputer` hashes them. Re-indenting a block therefore keeps
//! its lines matched instead of re-emitting every one of them as a deletion and
//! an insertion, which is the difference between a five-line insertion and a
//! thirty-line wall for the commonest edit a coding agent makes.

use std::fmt::Write;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use similar::{
    Algorithm, ChangeTag, DiffOp, InlineChangeMode, InlineChangeOptions, TextDiff,
    capture_diff_slices, group_diff_ops,
};

const CONTEXT_LINES: usize = 3;
const ALGORITHM: Algorithm = Algorithm::Patience;
/// Below `similar`'s default of `0.5`, inline refinement gives up on exactly
/// the lopsided replacements worth refining: a line rewritten around a kept
/// core, or a handful of removals answered by many more insertions.
const INLINE_MIN_RATIO: f32 = 0.25;
/// An unchanged run this short between two changed ones reads as noise rather
/// than as something kept. VS Code drops it in
/// `removeVeryShortMatchingTextBetweenLongDiffs`; `similar` has no equivalent.
const MIN_EQUAL_RUN: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffSpan {
    pub text: String,
    pub emphasized: bool,
}

impl DiffSpan {
    fn plain(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            emphasized: false,
        }
    }

    fn one(text: &str) -> Vec<Self> {
        vec![Self::plain(text)]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiffLine {
    Unchanged(String),
    /// Two lines the alignment matched whose bytes still differ, which after a
    /// trimmed-key alignment can only be indentation or trailing whitespace.
    /// One rendered row; [`unified_text`] still writes the `-`/`+` pair so the
    /// patch it produces stays a faithful record of the bytes.
    Reindented {
        before: Vec<DiffSpan>,
        after: Vec<DiffSpan>,
    },
    Added(Vec<DiffSpan>),
    Removed(Vec<DiffSpan>),
}

impl DiffLine {
    /// Whether the line occupies a row on the before side, the after side, or
    /// both, which is what a renderer numbers its gutter from.
    pub fn sides(&self) -> (bool, bool) {
        match self {
            Self::Unchanged(_) | Self::Reindented { .. } => (true, true),
            Self::Added(_) => (false, true),
            Self::Removed(_) => (true, false),
        }
    }
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

/// The refinement `similar` runs inside a replaced range, tuned to match what
/// VS Code does after its own character-level pass: whole-word units rather
/// than characters (`extendDiffsToEntireWordIfAppropriate`) and a semantic
/// cleanup that drops short matches (`removeShortMatches`).
fn inline_options() -> InlineChangeOptions {
    let mut options = InlineChangeOptions::new();
    options
        .algorithm(ALGORITHM)
        .mode(InlineChangeMode::UnicodeWords)
        .semantic_cleanup(true)
        .min_ratio(INLINE_MIN_RATIO);
    options
}

pub fn compute_hunks(before: &str, after: &str) -> Vec<DiffHunk> {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let before_keys: Vec<&str> = before_lines.iter().map(|line| line.trim()).collect();
    let after_keys: Vec<&str> = after_lines.iter().map(|line| line.trim()).collect();

    let ops = capture_diff_slices(ALGORITHM, &before_keys, &after_keys);
    let ops = mark_whitespace_changes(ops, &before_lines, &after_lines);
    group_diff_ops(ops, CONTEXT_LINES)
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let mut lines = Vec::new();
            for op in &group {
                push_op(&mut lines, *op, &before_lines, &after_lines);
            }
            Some(DiffHunk {
                before_start: first.old_range().start + 1,
                after_start: first.new_range().start + 1,
                lines,
            })
        })
        .collect()
}

/// Splits every equal run at the lines whose raw bytes differ, so a change made
/// only of whitespace still reaches the grouper as a change.
///
/// Without this, re-indenting a block produces no hunk at all: the trimmed keys
/// the alignment ran on are identical, and `group_diff_ops` keeps only the
/// groups that contain one.
fn mark_whitespace_changes(ops: Vec<DiffOp>, before: &[&str], after: &[&str]) -> Vec<DiffOp> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        else {
            out.push(op);
            continue;
        };
        let differs = |offset: usize| before[old_index + offset] != after[new_index + offset];
        let mut start = 0;
        while start < len {
            let changed = differs(start);
            let mut end = start + 1;
            while end < len && differs(end) == changed {
                end += 1;
            }
            out.push(match changed {
                true => DiffOp::Replace {
                    old_index: old_index + start,
                    old_len: end - start,
                    new_index: new_index + start,
                    new_len: end - start,
                },
                false => DiffOp::Equal {
                    old_index: old_index + start,
                    new_index: new_index + start,
                    len: end - start,
                },
            });
            start = end;
        }
    }
    out
}

fn push_op(out: &mut Vec<DiffLine>, op: DiffOp, before: &[&str], after: &[&str]) {
    match op {
        DiffOp::Equal {
            old_index,
            new_index,
            len,
        } => {
            for offset in 0..len {
                let (old, new) = (before[old_index + offset], after[new_index + offset]);
                out.push(match old == new {
                    true => DiffLine::Unchanged(old.to_owned()),
                    false => reindented(old, new),
                });
            }
        }
        DiffOp::Delete {
            old_index, old_len, ..
        } => out.extend(
            before[old_index..old_index + old_len]
                .iter()
                .map(|line| DiffLine::Removed(DiffSpan::one(line))),
        ),
        DiffOp::Insert {
            new_index, new_len, ..
        } => out.extend(
            after[new_index..new_index + new_len]
                .iter()
                .map(|line| DiffLine::Added(DiffSpan::one(line))),
        ),
        DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } => push_replace(
            out,
            &before[old_index..old_index + old_len],
            &after[new_index..new_index + new_len],
        ),
    }
}

/// The two sides of a replaced range, refined against each other so a rewritten
/// line marks only the words that moved.
///
/// The range is diffed again on raw lines because the alignment above ran on
/// trimmed keys, which says which lines correspond but not what within them
/// changed.
fn push_replace(out: &mut Vec<DiffLine>, before: &[&str], after: &[&str]) {
    if is_reindent(before, after) {
        out.extend(before.iter().zip(after).map(|(old, new)| match old == new {
            true => DiffLine::Unchanged((*old).to_owned()),
            false => reindented(old, new),
        }));
        return;
    }

    let (old_text, new_text) = (joined(before), joined(after));
    let diff = TextDiff::configure()
        .algorithm(ALGORITHM)
        .diff_lines(&old_text, &new_text);

    for change in diff.iter_all_inline_changes_with_options(inline_options()) {
        let mut spans: Vec<DiffSpan> = change
            .iter_strings_lossy()
            .map(|(emphasized, text)| DiffSpan {
                text: text.trim_end_matches('\n').to_owned(),
                emphasized,
            })
            .collect();
        merge_short_equal_runs(&mut spans);
        out.push(match change.tag() {
            ChangeTag::Equal => {
                DiffLine::Unchanged(spans.into_iter().map(|span| span.text).collect())
            }
            ChangeTag::Delete => DiffLine::Removed(spans),
            ChangeTag::Insert => DiffLine::Added(spans),
        });
    }
}

fn joined(lines: &[&str]) -> String {
    lines.iter().fold(String::new(), |mut out, line| {
        out.push_str(line);
        out.push('\n');
        out
    })
}

/// Whether a replaced range is nothing but whitespace moving: the same lines
/// in the same order, each one agreeing with its counterpart once trimmed.
fn is_reindent(before: &[&str], after: &[&str]) -> bool {
    before.len() == after.len()
        && before
            .iter()
            .zip(after)
            .all(|(old, new)| old.trim() == new.trim())
}

/// Two lines that agree once trimmed, so the only thing to mark is the
/// whitespace that differs. Computed directly rather than by diffing, because
/// the shape is known: the same core between possibly different margins.
fn reindented(before: &str, after: &str) -> DiffLine {
    let side = |line: &str, other: &str| {
        let (lead, core, trail) = split_margins(line);
        let (other_lead, _, other_trail) = split_margins(other);
        [
            DiffSpan {
                text: lead.to_owned(),
                emphasized: lead != other_lead,
            },
            DiffSpan::plain(core),
            DiffSpan {
                text: trail.to_owned(),
                emphasized: trail != other_trail,
            },
        ]
        .into_iter()
        .filter(|span| !span.text.is_empty())
        .collect()
    };
    DiffLine::Reindented {
        before: side(before, after),
        after: side(after, before),
    }
}

fn split_margins(line: &str) -> (&str, &str, &str) {
    let core = line.trim();
    let Some(start) = line.find(core).filter(|_| !core.is_empty()) else {
        return (line, "", "");
    };
    (&line[..start], core, &line[start + core.len()..])
}

/// Absorbs an unchanged run too short to read as kept text into the changed
/// runs either side of it, so a rewritten line marks one region instead of
/// scattering highlights across the characters two versions happen to share.
fn merge_short_equal_runs(spans: &mut Vec<DiffSpan>) {
    for index in 1..spans.len().saturating_sub(1) {
        let bridged = spans[index - 1].emphasized
            && spans[index + 1].emphasized
            && !spans[index].emphasized
            && spans[index].text.chars().count() < MIN_EQUAL_RUN;
        if bridged {
            spans[index].emphasized = true;
        }
    }
    coalesce(spans);
}

fn coalesce(spans: &mut Vec<DiffSpan>) {
    let mut merged: Vec<DiffSpan> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if last.emphasized == span.emphasized => last.text.push_str(&span.text),
            _ => merged.push(span),
        }
    }
    *spans = merged;
}

/// Added and removed line counts, in the units git reports. Walks the raw
/// changes rather than `compute_hunks`, whose trimmed alignment deliberately
/// answers a different question: what a reader should look at, not how many
/// bytes moved.
pub fn stat(before: &str, after: &str) -> String {
    let (added, removed) = TextDiff::from_lines(before, after).iter_all_changes().fold(
        (0, 0),
        |(added, removed), change| match change.tag() {
            ChangeTag::Insert => (added + 1, removed),
            ChangeTag::Delete => (added, removed + 1),
            ChangeTag::Equal => (added, removed),
        },
    );
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

/// The patch a reader and a model both see. A re-indented line is written as
/// the `-`/`+` pair it really is, so the text stays a faithful record of the
/// bytes even where the renderer draws it as one row.
pub fn unified_text(before: &str, after: &str, summary: &str, display_path: &str) -> String {
    let mut out = format!("{summary}\n--- {display_path}\n+++ {display_path}");
    let write_change = |out: &mut String, prefix: &str, spans: &[DiffSpan]| {
        let _ = write!(out, "\n{prefix}");
        for span in spans {
            out.push_str(&span.text);
        }
    };
    for hunk in compute_hunks(before, after) {
        out.push('\n');
        for line in &hunk.lines {
            match line {
                DiffLine::Unchanged(text) => {
                    let _ = write!(out, "\n  {text}");
                }
                DiffLine::Reindented { before, after } => {
                    write_change(&mut out, "- ", before);
                    write_change(&mut out, "+ ", after);
                }
                DiffLine::Removed(spans) => write_change(&mut out, "- ", spans),
                DiffLine::Added(spans) => write_change(&mut out, "+ ", spans),
            }
        }
    }
    out
}

/// The character ranges a renderer paints in the emphasis colour, for the side
/// of the line it is drawing.
pub fn emphasis_ranges(spans: &[DiffSpan]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for span in spans {
        let end = start + span.text.chars().count();
        if span.emphasized && end > start {
            ranges.push(start..end);
        }
        start = end;
    }
    ranges
}

pub fn span_text(spans: &[DiffSpan]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const REPLACED_LINE_STAT: &str = "+2 -1";
    const REINDENT_ALIGNS: &str = "a re-indented line must stay aligned, not be re-emitted";
    const PATCH_ROUND_TRIPS: &str = "a patch must rebuild both sides byte for byte";

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
            DiffLine::Unchanged(text) => text.clone(),
            DiffLine::Reindented { after, .. } => span_text(after),
            DiffLine::Added(spans) | DiffLine::Removed(spans) => span_text(spans),
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

    /// The edit this whole module is shaped around: a block wrapped in a
    /// conditional, which shifts every line inside it by one level.
    #[test]
    fn wrapping_a_block_reindents_rather_than_rewrites() {
        let before = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
        let after = "fn f() {\n    if cond {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n    }\n}\n";

        let lines = &compute_hunks(before, after)[0].lines;
        let bodies: Vec<String> = lines
            .iter()
            .filter(|line| matches!(line, DiffLine::Reindented { .. }))
            .map(line_text)
            .collect();

        assert!(
            !lines
                .iter()
                .any(|line| matches!(line, DiffLine::Removed(_))),
            "{REINDENT_ALIGNS}: {lines:?}"
        );
        assert_eq!(
            bodies,
            vec![
                "        let a = 1;",
                "        let b = 2;",
                "        let c = 3;",
                "    }"
            ],
            "{REINDENT_ALIGNS}"
        );
    }

    #[test]
    fn a_reindented_line_marks_only_the_margin() {
        let DiffLine::Reindented { before, after } = reindented("  keep", "      keep") else {
            unreachable!("constructed as a reindent")
        };

        assert_eq!(emphasis_ranges(&before), vec![0..2]);
        assert_eq!(emphasis_ranges(&after), vec![0..6]);
        assert_eq!(span_text(&after), "      keep");
    }

    /// Every hunk must rebuild both snapshots over the range it covers, or the
    /// patch a model reads would not describe the bytes on disk.
    #[test_case("fn f() {\n    let a = 1;\n}\n", "fn f() {\n    if c {\n        let a = 1;\n    }\n}\n" ; "wrapped block")]
    #[test_case("a\nb\nc\n", "a\nB\nc\n" ; "replaced line")]
    #[test_case("", "hello\nworld\n" ; "new file")]
    #[test_case("hello\nworld\n", "" ; "deleted file")]
    #[test_case("a\n  b\n", "a\n\tb\n" ; "retabbed")]
    #[test_case("keep \n", "keep\n" ; "trailing space dropped")]
    fn a_patch_rebuilds_both_sides(before: &str, after: &str) {
        let patch = unified_text(before, after, "s", "p");
        let side = |prefixes: [&str; 2]| {
            patch
                .lines()
                .skip(3)
                .filter_map(|line| {
                    prefixes
                        .iter()
                        .find_map(|prefix| line.strip_prefix(prefix))
                        .map(|text| format!("{text}\n"))
                })
                .collect::<String>()
        };

        assert_eq!(side(["- ", "  "]), before, "{PATCH_ROUND_TRIPS}");
        assert_eq!(side(["+ ", "  "]), after, "{PATCH_ROUND_TRIPS}");
    }

    #[test]
    fn a_short_kept_run_between_two_changes_is_absorbed() {
        let mut spans = vec![
            DiffSpan {
                text: "old".into(),
                emphasized: true,
            },
            DiffSpan::plain("_"),
            DiffSpan {
                text: "new".into(),
                emphasized: true,
            },
        ];
        merge_short_equal_runs(&mut spans);

        assert_eq!(
            spans,
            vec![DiffSpan {
                text: "old_new".into(),
                emphasized: true
            }]
        );
    }

    #[test]
    fn a_long_kept_run_survives() {
        let mut spans = vec![
            DiffSpan {
                text: "old".into(),
                emphasized: true,
            },
            DiffSpan::plain(" kept "),
            DiffSpan {
                text: "new".into(),
                emphasized: true,
            },
        ];
        let original = spans.clone();
        merge_short_equal_runs(&mut spans);

        assert_eq!(spans, original);
    }

    #[test]
    fn a_leading_margin_is_never_absorbed() {
        let mut spans = vec![
            DiffSpan::plain("  "),
            DiffSpan {
                text: "x".into(),
                emphasized: true,
            },
        ];
        let original = spans.clone();
        merge_short_equal_runs(&mut spans);

        assert_eq!(spans, original);
    }

    /// A rewritten line keeps the words it shares, which is what tells a reader
    /// the line was edited rather than replaced.
    #[test]
    fn a_rewritten_line_marks_the_words_that_moved() {
        let lines = &compute_hunks("let total = a + b;\n", "let total = a + c;\n")[0].lines;
        let DiffLine::Added(spans) = &lines[1] else {
            panic!("second line of a one-line replacement is the addition: {lines:?}")
        };

        assert!(
            spans.iter().any(|span| span.emphasized && span.text == "c"),
            "only the word that changed carries emphasis: {spans:?}"
        );
        assert_eq!(span_text(spans), "let total = a + c;");
    }
}
