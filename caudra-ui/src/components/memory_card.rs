//! The transcript card of a `memory` browse: the notes a read returned, or the
//! tag index a list did.
//!
//! A note's own row is what the card promises — its name, what its body costs,
//! and the tags that reach it — so those rows are pinned and the bodies are
//! what a budget takes away. A collapsed card is then the index of what came
//! back rather than the opening lines of whichever note happened to be first.

use std::path::PathBuf;

use caudra_agent::{
    MEMORY_TAG_SEPARATOR, MemoryNote, MemoryNoteEntry, MemoryOrigin, MemoryOutput, MemoryTagGroup,
};
use caudra_providers::token_label;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::code_view::{RowTarget, body_window, truncation_line};
use crate::components::escape_terminal_controls;
use crate::markdown::text_to_painted;
use crate::theme;

/// Columns between a name and what measures it.
const GAP: &str = "  ";
/// What a note's body and a tag's notes sit under, so the row above stays the
/// thing being read against.
const INDENT: &str = "  ";
const INDENT_WIDTH: u16 = 2;
const TAG_COUNT_OPEN: &str = " (";
const TAG_COUNT_CLOSE: char = ')';

/// The card's lines, what each of them answers for, and whether a body was
/// held back.
pub(crate) fn render(
    output: &MemoryOutput,
    budget: usize,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>, bool) {
    let mut card = Card::default();
    card.push_notices(output.notices());
    match output {
        MemoryOutput::Notes { notes, .. } => card.push_notes(notes, budget, width),
        MemoryOutput::Index { groups, .. } => card.push_index(groups, budget),
    }
    (card.lines, card.rows, card.truncated)
}

/// The note a row belongs to, when it is one a click can open. Remote notes
/// are held by reference and have no file on this host, so they answer `None`
/// and the click falls through to the card's own control.
pub(crate) fn note_path(output: &MemoryOutput, target: RowTarget) -> Option<PathBuf> {
    origins(output).get(target.0)?.path().map(PathBuf::from)
}

/// Every note the card drew, in the order it drew them. Both the renderer and
/// the click path index this, so a target means the same note to each.
fn origins(output: &MemoryOutput) -> Vec<&MemoryOrigin> {
    match output {
        MemoryOutput::Notes { notes, .. } => notes.iter().map(|note| &note.origin).collect(),
        MemoryOutput::Index { groups, .. } => groups
            .iter()
            .flat_map(|group| group.notes.iter().map(|note| &note.origin))
            .collect(),
    }
}

#[derive(Default)]
struct Card {
    lines: Vec<Line<'static>>,
    /// Parallel to `lines`, so a click resolves after the wrap has changed how
    /// many rows a line became.
    rows: Vec<Option<RowTarget>>,
    truncated: bool,
}

impl Card {
    fn push(&mut self, line: Line<'static>, target: Option<RowTarget>) {
        self.lines.push(line);
        self.rows.push(target);
    }

    fn push_notices(&mut self, notices: &[String]) {
        let dim = theme::current().tool_dim;
        for notice in notices {
            self.push(
                Line::from(Span::styled(escape_terminal_controls(notice), dim)),
                None,
            );
        }
    }

    /// Bodies are painted before any of them is drawn, because a markdown body
    /// only has rows once the renderer has run and the budget is taken on the
    /// rows a reader would read.
    fn push_notes(&mut self, notes: &[MemoryNote], budget: usize, width: u16) {
        let bodies: Vec<Vec<Line<'static>>> = notes
            .iter()
            .map(|note| body_lines(&note.body, width))
            .collect();
        let separators = notes.len().saturating_sub(1);
        let pinned = self.lines.len() + notes.len() + separators;
        let (mut room, hidden) = body_window(
            bodies.iter().map(Vec::len).sum(),
            budget.saturating_sub(pinned),
        );
        for (index, (note, body)) in notes.iter().zip(bodies).enumerate() {
            let target = clickable(&note.origin, index);
            if index > 0 {
                self.push(Line::default(), target);
            }
            self.push(note_headline(note), target);
            let shown = body.len().min(room);
            room -= shown;
            for line in body.into_iter().take(shown) {
                self.push(indented(line), target);
            }
        }
        self.finish(hidden);
    }

    fn push_index(&mut self, groups: &[MemoryTagGroup], budget: usize) {
        let entries: usize = groups.iter().map(|group| 1 + group.notes.len()).sum();
        let (room, hidden) = body_window(entries, budget.saturating_sub(self.lines.len()));
        let tokens_width = column_width(groups, |note| token_label(note.tokens).width());
        let name_width = column_width(groups, |note| note.name.width());
        let mut drawn = 0;
        let mut index = 0;
        for group in groups {
            if drawn >= room {
                break;
            }
            self.push(group_headline(group), None);
            drawn += 1;
            for note in &group.notes {
                if drawn < room {
                    self.push(
                        index_row(note, name_width, tokens_width),
                        clickable(&note.origin, index),
                    );
                    drawn += 1;
                }
                index += 1;
            }
        }
        self.finish(hidden);
    }

    fn finish(&mut self, hidden: usize) {
        if hidden == 0 {
            return;
        }
        self.push(truncation_line(hidden), None);
        self.truncated = true;
    }
}

/// Every row of a note answers for it, the way a batch child's rows do: an open
/// note is one thing to click, not a header with unaddressed text beneath it.
fn clickable(origin: &MemoryOrigin, index: usize) -> Option<RowTarget> {
    origin.path().is_some().then_some(RowTarget(index))
}

/// The note's body as the document it is. Painted at the width it will sit at
/// so its own wrapping is the one the reader sees.
fn body_lines(body: &str, width: u16) -> Vec<Line<'static>> {
    if body.trim().is_empty() {
        return Vec::new();
    }
    let style = theme::current().assistant;
    let (painted, _) = text_to_painted(
        body,
        "",
        style,
        style,
        width.saturating_sub(INDENT_WIDTH),
        Some(caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        Vec::new(),
    );
    painted.lines
}

fn indented(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(INDENT));
    line
}

fn note_headline(note: &MemoryNote) -> Line<'static> {
    let theme = theme::current();
    let mut spans = Vec::from([
        Span::styled(escape_terminal_controls(&note.name), theme.tool_path),
        Span::styled(format!("{GAP}{}", token_label(note.tokens)), theme.tool_dim),
    ]);
    if !note.tags.is_empty() {
        spans.push(Span::styled(
            format!(
                "{GAP}{}",
                escape_terminal_controls(&note.tags.join(MEMORY_TAG_SEPARATOR))
            ),
            theme.tool_dim,
        ));
    }
    Line::from(spans)
}

fn group_headline(group: &MemoryTagGroup) -> Line<'static> {
    let theme = theme::current();
    Line::from(Vec::from([
        Span::styled(escape_terminal_controls(&group.tag), theme.tool_prefix),
        Span::styled(
            format!("{TAG_COUNT_OPEN}{}{TAG_COUNT_CLOSE}", group.notes.len()),
            theme.tool_dim,
        ),
    ]))
}

/// The name pads in a span of its own so a copy of the row does not carry the
/// column with it.
fn index_row(note: &MemoryNoteEntry, name_width: usize, tokens_width: usize) -> Line<'static> {
    let theme = theme::current();
    let name = escape_terminal_controls(&note.name);
    let pad = name_width.saturating_sub(name.width());
    Line::from(Vec::from([
        Span::raw(INDENT),
        Span::styled(name, theme.tool_path),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            format!("{GAP}{:>tokens_width$}", token_label(note.tokens)),
            theme.tool_dim,
        ),
    ]))
}

fn column_width(groups: &[MemoryTagGroup], measure: impl Fn(&MemoryNoteEntry) -> usize) -> usize {
    groups
        .iter()
        .flat_map(|group| group.notes.iter())
        .map(measure)
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use caudra_agent::MemoryOutput;
    use test_case::test_case;

    use super::*;

    const WIDTH: u16 = 80;
    const BODY: &str = "first\n\nsecond\n\nthird";
    const BODY_ROWS: usize = 5;
    const NOTE: &str = "gotchas.md";
    const OTHER: &str = "architecture.md";
    const TAG: &str = "workcell";
    const REFERENCE: &str = "memory-aaaa";
    const REVISION: &str = "bbbb";
    const PINNED_MSG: &str = "a note's own row is never what a budget takes";
    const CLICK_MSG: &str = "a local note takes the click, a remote one cannot";

    fn file_note(name: &str) -> MemoryNote {
        MemoryNote {
            name: name.to_owned(),
            tokens: 1,
            tags: Vec::from([TAG.to_owned()]),
            origin: MemoryOrigin::File {
                path: format!("/notes/{name}"),
            },
            body: BODY.to_owned(),
        }
    }

    fn remote(name: &str) -> MemoryNote {
        MemoryNote {
            origin: MemoryOrigin::Document {
                reference: REFERENCE.to_owned(),
                revision: REVISION.to_owned(),
            },
            ..file_note(name)
        }
    }

    fn notes(notes: Vec<MemoryNote>, notices: Vec<String>) -> MemoryOutput {
        MemoryOutput::Notes {
            directory: None,
            notes,
            notices,
        }
    }

    fn index(groups: Vec<MemoryTagGroup>) -> MemoryOutput {
        MemoryOutput::Index {
            directory: None,
            groups,
            notices: Vec::new(),
        }
    }

    fn entry(name: &str, tokens: u32) -> MemoryNoteEntry {
        MemoryNoteEntry {
            name: name.to_owned(),
            tokens,
            origin: MemoryOrigin::File {
                path: format!("/notes/{name}"),
            },
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn rendered(output: &MemoryOutput, budget: usize) -> Vec<String> {
        render(output, budget, WIDTH).0.iter().map(text).collect()
    }

    /// The point of the card: a budget that cannot fit the bodies still shows
    /// which notes came back.
    #[test]
    fn a_collapsed_read_keeps_every_note_row_and_drops_the_bodies() {
        let output = notes(Vec::from([file_note(NOTE), file_note(OTHER)]), Vec::new());
        let (lines, _, truncated) = render(&output, 3, WIDTH);
        let drawn: Vec<String> = lines.iter().map(text).collect();

        assert!(truncated);
        assert!(drawn[0].starts_with(NOTE), "{PINNED_MSG}: {drawn:?}");
        assert!(
            drawn.iter().any(|row| row.starts_with(OTHER)),
            "{PINNED_MSG}: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|row| row.contains("first")),
            "a body is what a budget takes: {drawn:?}"
        );
    }

    #[test]
    fn an_expanded_read_draws_each_body_whole_and_announces_nothing() {
        let output = notes(Vec::from([file_note(NOTE)]), Vec::new());
        let (lines, _, truncated) = render(&output, usize::MAX, WIDTH);

        assert!(!truncated);
        assert_eq!(lines.len(), 1 + BODY_ROWS);
        for word in ["first", "second", "third"] {
            assert!(lines.iter().any(|line| text(line).contains(word)));
        }
    }

    #[test_case(file_note(NOTE), Some(RowTarget(0)) ; "a_note_on_this_host")]
    #[test_case(remote(NOTE), None ; "a_note_held_by_reference")]
    fn a_row_answers_for_the_note_a_click_could_open(
        note: MemoryNote,
        expected: Option<RowTarget>,
    ) {
        let output = notes(Vec::from([note]), Vec::new());
        let (_, rows, _) = render(&output, usize::MAX, WIDTH);

        assert!(rows.iter().all(|row| *row == expected), "{CLICK_MSG}");
    }

    #[test]
    fn a_click_resolves_to_the_note_its_row_named() {
        let output = notes(Vec::from([file_note(NOTE), file_note(OTHER)]), Vec::new());

        assert_eq!(
            note_path(&output, RowTarget(1)),
            Some(PathBuf::from(format!("/notes/{OTHER}")))
        );
        assert_eq!(note_path(&output, RowTarget(9)), None, "{CLICK_MSG}");
    }

    /// An index is flattened across its groups, so a note filed under the
    /// second tag still resolves to itself.
    #[test]
    fn an_indexed_click_counts_across_tag_groups() {
        let output = index(Vec::from([
            MemoryTagGroup {
                tag: TAG.to_owned(),
                notes: Vec::from([entry(NOTE, 1)]),
            },
            MemoryTagGroup {
                tag: "ui".to_owned(),
                notes: Vec::from([entry(OTHER, 2)]),
            },
        ]));

        assert_eq!(
            note_path(&output, RowTarget(1)),
            Some(PathBuf::from(format!("/notes/{OTHER}")))
        );
    }

    /// The measurement is right-aligned, so what lines up is where the labels
    /// end, however wide the name beside them is.
    #[test]
    fn an_index_aligns_the_token_column_under_its_own_tag() {
        let output = index(Vec::from([MemoryTagGroup {
            tag: TAG.to_owned(),
            notes: Vec::from([entry("a.md", 1), entry("a-much-longer-name.md", 2_000)]),
        }]));
        let drawn = rendered(&output, usize::MAX);

        let ends: Vec<usize> = drawn[1..].iter().map(|row| row.width()).collect();
        assert_eq!(ends[0], ends[1], "{drawn:?}");
    }

    #[test]
    fn a_notice_leads_the_card_it_qualifies() {
        const NOTICE: &str = "warning: unreadable memory files: a.md";
        let output = notes(Vec::from([file_note(NOTE)]), Vec::from([NOTICE.to_owned()]));
        let drawn = rendered(&output, usize::MAX);

        assert_eq!(drawn[0], NOTICE);
        assert!(drawn[1].starts_with(NOTE), "{drawn:?}");
    }

    /// An empty browse is its notice and nothing else, so the card says what
    /// happened rather than drawing a note that is not there.
    #[test]
    fn an_empty_browse_draws_its_notice_alone() {
        const NOTICE: &str = "No memories yet.";
        let output = index(Vec::new());
        let MemoryOutput::Index {
            groups, directory, ..
        } = output
        else {
            unreachable!()
        };
        let output = MemoryOutput::Index {
            directory,
            groups,
            notices: Vec::from([NOTICE.to_owned()]),
        };

        assert_eq!(
            rendered(&output, usize::MAX),
            Vec::from([NOTICE.to_owned()])
        );
    }
}
