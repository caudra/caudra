//! Renders the two sides of a change as the rows a diff tab shows.
//!
//! The result is deliberately a unified diff rather than a side-by-side one:
//! the editor already knows how to scroll, search and select a single column of
//! text, and a diff tab is worth having only if it inherits all of that.
//!
//! The hunks come from `caudra-diff`, the same model the transcript's edit cards
//! are drawn from, so both surfaces agree on what changed and on which part of
//! a line changed within it.

use std::ops::Range;

use caudra_diff::{DiffLine, compute_hunks, emphasis_ranges, span_text};
use caudra_highlight::{Highlighter, StyledSegment};

use crate::editor::DiffKind;

const IDENTICAL: &str = "@@ no changes @@";
const GAP: &str = "@@ \u{2026} @@";
/// Past this many lines on a side, highlighting it whole to colour the handful
/// of rows a diff actually shows costs more than the colour is worth.
const MAX_HIGHLIGHT_LINES: usize = 20_000;

/// One row of a rendered diff: the text without any `-`/`+` prefix, which side
/// or sides it belongs to, and the characters within it that changed.
#[derive(Debug, Default, PartialEq)]
pub struct DiffRow {
    pub text: String,
    pub kind: DiffKind,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub emphasis: Vec<Range<usize>>,
    pub segments: Vec<StyledSegment>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    pub rows: Vec<DiffRow>,
}

impl Diff {
    fn push(&mut self, text: String, kind: DiffKind) {
        self.rows.push(DiffRow {
            text,
            kind,
            ..DiffRow::default()
        });
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.text.clone()).collect()
    }
}

/// Syntect is stateful, so colouring line N needs lines `1..N` first. A diff
/// has two files, each with its own parser state, so each side is coloured by
/// its own highlighter and a row takes the colours of the side it came from.
///
/// Colouring a side whole is the price of a tab that shows only its hunks; it
/// is paid once when the tab opens rather than per viewport, because a
/// synthetic buffer's rows are not its file's rows and the viewport highlighter
/// has no way to map between them. `None` past the cap, where a flat diff costs
/// less than the wait.
fn side_colours(path: &str, content: &str) -> Option<Vec<Vec<StyledSegment>>> {
    if content.lines().count() > MAX_HIGHLIGHT_LINES {
        return None;
    }
    let mut highlighter = Highlighter::for_path(path);
    Some(
        content
            .lines()
            .map(|line| highlighter.highlight_line(&format!("{line}\n")))
            .collect(),
    )
}

/// The colours of a 1-indexed line on one side.
fn colours_at(side: &[Vec<StyledSegment>], line: usize) -> Vec<StyledSegment> {
    side.get(line - 1).cloned().unwrap_or_default()
}

type SideColours = (Vec<Vec<StyledSegment>>, Vec<Vec<StyledSegment>>);

pub fn unified(path: &str, old: &str, new: &str) -> Diff {
    let hunks = compute_hunks(old, new);
    let mut diff = Diff::default();
    if hunks.is_empty() {
        diff.push(IDENTICAL.to_owned(), DiffKind::Header);
        return diff;
    }

    let colours = side_colours(path, old).zip(side_colours(path, new));
    for (index, hunk) in hunks.iter().enumerate() {
        if index > 0 {
            diff.push(GAP.to_owned(), DiffKind::Header);
        }
        let mut cursor = (hunk.before_start, hunk.after_start);
        for line in &hunk.lines {
            diff.rows.push(row(line, &mut cursor, colours.as_ref()));
        }
    }
    diff
}

fn row(line: &DiffLine, cursor: &mut (usize, usize), colours: Option<&SideColours>) -> DiffRow {
    let (before, after) = *cursor;
    let (on_before, on_after) = line.sides();
    cursor.0 += usize::from(on_before);
    cursor.1 += usize::from(on_after);

    let (kind, text, emphasis, from_before) = match line {
        DiffLine::Unchanged(text) => (DiffKind::Context, text.clone(), Vec::new(), false),
        DiffLine::Reindented { after, .. } => (
            DiffKind::Reindented,
            span_text(after),
            emphasis_ranges(after),
            false,
        ),
        DiffLine::Added(spans) => (
            DiffKind::Added,
            span_text(spans),
            emphasis_ranges(spans),
            false,
        ),
        DiffLine::Removed(spans) => (
            DiffKind::Removed,
            span_text(spans),
            emphasis_ranges(spans),
            true,
        ),
    };

    let segments = colours.map_or_else(Vec::new, |(old, new)| match from_before {
        true => colours_at(old, before),
        false => colours_at(new, after),
    });

    DiffRow {
        text,
        kind,
        before: on_before.then_some(before),
        after: on_after.then_some(after),
        emphasis,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{DiffKind, IDENTICAL, unified};

    const PATH: &str = "x.rs";
    const HEADER_FIRST: &str = "a hunk that is not the first must be marked as a jump";
    const NO_CHANGES: &str = "identical sides must render as a single notice";
    const KIND_WRONG: &str = "a row's kind must agree with the side it came from";
    const NO_PREFIX: &str = "a row's text is the line, not the patch line";
    const EMPHASIS: &str = "a changed row must mark the characters that changed";
    const REINDENT: &str = "a re-indented line is one row, marking only its margin";

    fn kinds(diff: &super::Diff) -> Vec<DiffKind> {
        diff.rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn identical_sides_render_a_single_notice() {
        let diff = unified(PATH, "a\nb\n", "a\nb\n");
        assert_eq!(diff.lines(), vec![IDENTICAL.to_owned()], "{NO_CHANGES}");
        assert_eq!(kinds(&diff), vec![DiffKind::Header], "{NO_CHANGES}");
    }

    #[test]
    fn a_replaced_line_renders_a_removal_and_an_addition() {
        let diff = unified(PATH, "a\nb\nc\n", "a\nB\nc\n");
        let rows: Vec<(&str, DiffKind)> = diff
            .rows
            .iter()
            .map(|row| (row.text.as_str(), row.kind))
            .collect();

        assert!(rows.contains(&("b", DiffKind::Removed)), "{NO_PREFIX}");
        assert!(rows.contains(&("B", DiffKind::Added)), "{NO_PREFIX}");
        assert!(rows.contains(&("a", DiffKind::Context)), "{NO_PREFIX}");
    }

    #[test]
    fn a_changed_row_carries_the_ranges_that_changed() {
        let diff = unified(PATH, "let a = 1;\n", "let a = 2;\n");
        let added = diff
            .rows
            .iter()
            .find(|row| row.kind == DiffKind::Added)
            .expect(KIND_WRONG);

        assert_eq!(added.emphasis, vec![8..9], "{EMPHASIS}: {added:?}");
    }

    #[test]
    fn a_reindented_line_is_one_row_marking_its_margin() {
        let diff = unified(PATH, "fn f() {\nwork();\n}\n", "fn f() {\n    work();\n}\n");
        let row = diff
            .rows
            .iter()
            .find(|row| row.kind == DiffKind::Reindented)
            .expect(REINDENT);

        assert_eq!(row.text, "    work();", "{REINDENT}");
        assert_eq!(row.emphasis, vec![0..4], "{REINDENT}");
        assert!(
            !kinds(&diff).contains(&DiffKind::Removed),
            "{REINDENT}: {:?}",
            kinds(&diff)
        );
    }

    #[test]
    fn every_row_numbers_the_sides_it_belongs_to() {
        let diff = unified(PATH, "one\ntwo\nthree\n", "one\nthree\nfour\n");
        for row in &diff.rows {
            let expected = match row.kind {
                DiffKind::Added => (false, true),
                DiffKind::Removed => (true, false),
                _ => (true, true),
            };
            assert_eq!(
                (row.before.is_some(), row.after.is_some()),
                expected,
                "{KIND_WRONG}: {row:?}"
            );
        }
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let diff = unified(PATH, "", "hello\nworld\n");
        assert_eq!(
            kinds(&diff),
            vec![DiffKind::Added, DiffKind::Added],
            "{KIND_WRONG}"
        );
    }

    #[test]
    fn a_deleted_file_is_all_removals() {
        let diff = unified(PATH, "hello\nworld\n", "");
        assert_eq!(
            kinds(&diff),
            vec![DiffKind::Removed, DiffKind::Removed],
            "{KIND_WRONG}"
        );
    }

    #[test]
    fn distant_edits_are_separated_by_a_jump() {
        let old: String = (0..40).map(|line| format!("line {line}\n")).collect();
        let new = old
            .replace("line 1\n", "changed 1\n")
            .replace("line 38\n", "changed 38\n");
        let headers = kinds(&unified(PATH, &old, &new))
            .iter()
            .filter(|kind| **kind == DiffKind::Header)
            .count();

        assert_eq!(headers, 1, "{HEADER_FIRST}");
    }

    #[test_case("a\n", "a\nb\n"; "append")]
    #[test_case("a\nb\n", "a\n"; "truncate")]
    #[test_case("", ""; "both empty")]
    fn a_diff_never_renders_a_bare_newline(old: &str, new: &str) {
        for line in unified(PATH, old, new).lines() {
            assert!(!line.contains('\n'), "{NO_PREFIX}");
        }
    }
}
