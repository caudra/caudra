//! Renders the two sides of a change as the rows a diff tab shows.
//!
//! The result is deliberately a unified diff rather than a side-by-side one:
//! the editor already knows how to scroll, search and select a single column of
//! text, and a diff tab is worth having only if it inherits all of that.
//!
//! The hunks come from `caudra-diff`, the same model the transcript's edit cards
//! are drawn from, so both surfaces agree on what changed and on which part of
//! a line changed within it.

use std::collections::HashMap;
use std::ops::Range;

use caudra_diff::{DiffHunk, DiffLine, compute_hunks, emphasis_ranges, span_text};
use caudra_highlight::{Highlighter, StyledSegment};
use caudra_workspace::{ScmDiffLine, ScmDiffLineKind};

use crate::editor::DiffKind;
use crate::editor::highlight::MAX_LOOKBACK;

const IDENTICAL: &str = "@@ no changes @@";
const GAP: &str = "@@ \u{2026} @@";
/// Past this many lines parsed, colouring costs more than the colour is worth.
/// Only what the hunks reach is parsed, so this is reached by a change scattered
/// through a file rather than merely by a large one.
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
    pub(super) fn push(&mut self, text: String, kind: DiffKind) {
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
/// A diff shows only its hunks, so only the lines those reach are parsed, each
/// from a bounded distance above itself. Colouring both sides whole is what a
/// tab used to cost: two seconds to put eight rows on screen for a one-line
/// change in a six-thousand-line file, because the price scaled with the file
/// rather than with the change. The lines a lookback did not truly fold over
/// are at risk only of being coloured as if a block comment or raw string that
/// opened above it had closed.
fn side_colours(path: &str, content: &str, windows: &[Range<usize>]) -> SideColours {
    let lines: Vec<&str> = content.lines().collect();
    let mut colours = HashMap::new();
    for window in windows {
        // Each window starts above what it is for, so it needs its own parser:
        // resuming the last one would be resuming it across the gap that made
        // this a separate window.
        let mut highlighter = Highlighter::for_path(path);
        for line in window.start..window.end.min(lines.len() + 1) {
            colours.insert(
                line,
                highlighter.highlight_line(&format!("{}\n", lines[line - 1])),
            );
        }
    }
    colours
}

/// The 1-indexed line range each hunk covers on each side, which is every line
/// a row of it can ask about.
fn touched(hunks: &[DiffHunk]) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for hunk in hunks {
        let (mut last_before, mut last_after) = (hunk.before_start, hunk.after_start);
        for line in &hunk.lines {
            let (on_before, on_after) = line.sides();
            last_before += usize::from(on_before);
            last_after += usize::from(on_after);
        }
        before.push(hunk.before_start..last_before);
        after.push(hunk.after_start..last_after);
    }
    (before, after)
}

/// What has to be parsed to colour `wanted`: every range reached back by the
/// lookback, and anything that then overlaps merged, since walking through a
/// short gap costs less than starting a parser over above the next hunk.
fn windows(wanted: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut windows: Vec<Range<usize>> = Vec::new();
    for range in wanted {
        let start = range.start.saturating_sub(MAX_LOOKBACK).max(1);
        match windows.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(range.end),
            _ => windows.push(start..range.end),
        }
    }
    windows
}

fn parsed_lines(windows: &[Range<usize>]) -> usize {
    windows.iter().map(|window| window.end - window.start).sum()
}

/// The colours of a 1-indexed line on one side.
fn colours_at(side: &SideColours, line: usize) -> Vec<StyledSegment> {
    side.get(&line).cloned().unwrap_or_default()
}

type SideColours = HashMap<usize, Vec<StyledSegment>>;
type Sides = (SideColours, SideColours);

pub fn unified(path: &str, old: &str, new: &str) -> Diff {
    let hunks = compute_hunks(old, new);
    let mut diff = Diff::default();
    if hunks.is_empty() {
        diff.push(IDENTICAL.to_owned(), DiffKind::Header);
        return diff;
    }

    let (before, after) = touched(&hunks);
    let (before, after) = (windows(&before), windows(&after));
    let affordable = parsed_lines(&before) + parsed_lines(&after) <= MAX_HIGHLIGHT_LINES;
    let colours = affordable.then(|| {
        (
            side_colours(path, old, &before),
            side_colours(path, new, &after),
        )
    });
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

pub fn structured(
    path: &str,
    old: &str,
    new: &str,
    lines: Vec<ScmDiffLine>,
    incomplete: bool,
) -> Diff {
    let before = lines
        .iter()
        .filter_map(|line| line.old_line.map(|line| line as usize..line as usize + 1))
        .collect::<Vec<_>>();
    let after = lines
        .iter()
        .filter_map(|line| line.new_line.map(|line| line as usize..line as usize + 1))
        .collect::<Vec<_>>();
    let (before, after) = (windows(&before), windows(&after));
    let affordable = parsed_lines(&before) + parsed_lines(&after) <= MAX_HIGHLIGHT_LINES;
    let colours = affordable.then(|| {
        (
            side_colours(path, old, &before),
            side_colours(path, new, &after),
        )
    });
    let mut diff = Diff::default();
    for line in lines {
        let kind = match line.kind {
            ScmDiffLineKind::File | ScmDiffLineKind::Binary => DiffKind::Header,
            ScmDiffLineKind::Context => DiffKind::Context,
            ScmDiffLineKind::Addition => DiffKind::Added,
            ScmDiffLineKind::Deletion => DiffKind::Removed,
        };
        let segments = colours
            .as_ref()
            .map_or_else(Vec::new, |(old, new)| match line.kind {
                ScmDiffLineKind::Deletion => line
                    .old_line
                    .map(|line| colours_at(old, line as usize))
                    .unwrap_or_default(),
                ScmDiffLineKind::Context | ScmDiffLineKind::Addition => line
                    .new_line
                    .map(|line| colours_at(new, line as usize))
                    .unwrap_or_default(),
                ScmDiffLineKind::File | ScmDiffLineKind::Binary => Vec::new(),
            });
        diff.rows.push(DiffRow {
            text: line.text,
            kind,
            before: line.old_line.map(|line| line as usize),
            after: line.new_line.map(|line| line as usize),
            emphasis: Vec::new(),
            segments,
        });
    }
    if incomplete {
        diff.push("@@ remote diff incomplete @@".to_owned(), DiffKind::Header);
    }
    diff
}

fn row(line: &DiffLine, cursor: &mut (usize, usize), colours: Option<&Sides>) -> DiffRow {
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
    use caudra_workspace::{ScmDiffLine, ScmDiffLineKind, WorkspacePath};
    use test_case::test_case;

    use super::{DiffKind, IDENTICAL, structured, unified, windows};

    const PATH: &str = "x.rs";
    const LARGE: usize = 5_000;
    const HEADER_FIRST: &str = "a hunk that is not the first must be marked as a jump";
    const NO_CHANGES: &str = "identical sides must render as a single notice";
    const KIND_WRONG: &str = "a row's kind must agree with the side it came from";
    const NO_PREFIX: &str = "a row's text is the line, not the patch line";
    const EMPHASIS: &str = "a changed row must mark the characters that changed";
    const REINDENT: &str = "a re-indented line is one row, marking only its margin";
    const NOT_COLOURED: &str = "a changed row must carry the colours of the line it came from";
    const WINDOW_SPLIT: &str = "hunks within a lookback of each other must be parsed in one pass";
    const WINDOW_MERGED: &str = "hunks a lookback apart must not drag the parse across the gap";
    const STRUCTURE_CHANGED: &str = "structured remote diff rows must be projected exactly";

    #[test]
    fn structured_rows_keep_protocol_kinds_numbers_and_continuation_notice() {
        let path = WorkspacePath::new(PATH).expect("valid path");
        let lines = vec![
            ScmDiffLine {
                path: path.clone(),
                kind: ScmDiffLineKind::File,
                change: None,
                old_line: None,
                new_line: None,
                text: "@@ hunk 1 @@".to_owned(),
            },
            ScmDiffLine {
                path,
                kind: ScmDiffLineKind::Deletion,
                change: None,
                old_line: Some(7),
                new_line: None,
                text: "old".to_owned(),
            },
        ];

        let diff = structured(PATH, "old\n", "new\n", lines, true);

        assert_eq!(diff.rows[0].kind, DiffKind::Header, "{STRUCTURE_CHANGED}");
        assert_eq!(diff.rows[1].kind, DiffKind::Removed, "{STRUCTURE_CHANGED}");
        assert_eq!(diff.rows[1].before, Some(7), "{STRUCTURE_CHANGED}");
        assert_eq!(diff.rows[1].after, None, "{STRUCTURE_CHANGED}");
        assert_eq!(
            diff.rows[2].text, "@@ remote diff incomplete @@",
            "{STRUCTURE_CHANGED}"
        );
    }

    fn kinds(diff: &super::Diff) -> Vec<DiffKind> {
        diff.rows.iter().map(|row| row.kind).collect()
    }

    /// The colours no longer come from parsing the file whole, so a change that
    /// nothing above it was parsed for is the case worth holding: a hunk at the
    /// bottom of a large file still has to arrive coloured.
    #[test]
    fn a_hunk_far_down_a_file_is_still_coloured() {
        let old: String = (0..LARGE)
            .map(|index| format!("let line{index} = {index};\n"))
            .collect();
        let last = LARGE - 1;
        let new = old.replace(
            &format!("let line{last} = {last};"),
            &format!("let line{last} = {LARGE};"),
        );

        let diff = unified(PATH, &old, &new);
        let added = diff
            .rows
            .iter()
            .find(|row| row.kind == DiffKind::Added)
            .expect("an added row");

        assert!(!added.segments.is_empty(), "{NOT_COLOURED}");
    }

    #[test]
    fn nearby_hunks_share_one_parse_and_distant_ones_do_not() {
        assert_eq!(windows(&[10..20, 30..40]), vec![1..40], "{WINDOW_SPLIT}");
        assert_eq!(
            windows(&[10..20, 10_000..10_010]).len(),
            2,
            "{WINDOW_MERGED}"
        );
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
