//! The transcript card of a `memory` call: the notes a read or a zoom into one
//! entry returned, the lines a view or any other zoom showed, the notes a
//! search found, or the tag index a stored session's list did.
//!
//! A note's own row is what the card promises — its name and what its body
//! costs — so those rows are pinned and the bodies are what a budget takes
//! away. A collapsed card is then the index of what came back rather than the
//! opening lines of whichever note happened to be first.
//!
//! Lines and hits are drawn from the fields the model's text is built from,
//! laid out the way a read or a grep is: the address in a numbered column and
//! the text hung beside it, so a long line never wraps back under its address.
//! The card adds no words of its own.

use std::path::PathBuf;

use caudra_agent::memory::search::terms;
use caudra_agent::{
    MEMORY_TAG_SEPARATOR, MemoryHit, MemoryLine, MemoryNote, MemoryNoteEntry, MemoryOutput,
    MemoryTagGroup,
};
use caudra_providers::token_label;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::code_view::{RowTarget, body_window, truncation_line};
use crate::components::{escape_terminal_controls, hanging_spans, highlight, term_ranges};
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
/// Between an address and the text it addresses, as between a line number
/// and its code.
const ADDRESS_GAP: &str = " ";

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
        MemoryOutput::Lines { lines, .. } => {
            card.push_lines(&output.heading(), lines, budget, width);
        }
        MemoryOutput::Hits { query, hits, .. } => {
            card.push_hits(&output.heading(), query, hits, budget, width);
        }
    }
    (card.lines, card.rows, card.truncated)
}

/// The note a row belongs to, when it is one a click can open. Remote notes
/// are held by reference and have no file on this host, so they answer `None`
/// and the click falls through to the card's own control.
pub(crate) fn note_path(output: &MemoryOutput, target: RowTarget) -> Option<PathBuf> {
    paths(output)
        .get(target.index())
        .copied()
        .flatten()
        .map(PathBuf::from)
}

/// The file of every note the card drew, in the order it drew them. Both the
/// renderer and the click path index this, so a target means the same note
/// to each.
fn paths(output: &MemoryOutput) -> Vec<Option<&str>> {
    match output {
        MemoryOutput::Notes { notes, .. } => notes.iter().map(|note| note.origin.path()).collect(),
        MemoryOutput::Index { groups, .. } => groups
            .iter()
            .flat_map(|group| group.notes.iter().map(|note| note.origin.path()))
            .collect(),
        MemoryOutput::Hits { hits, .. } => hits.iter().map(|hit| hit.path.as_deref()).collect(),
        MemoryOutput::Lines { .. } => Vec::new(),
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

    /// Text hung beside `gutter`, already broken to `width` so its later rows
    /// start under the text rather than under the gutter.
    fn push_hung(
        &mut self,
        gutter: Span<'static>,
        spans: Vec<Span<'static>>,
        width: u16,
        target: Option<RowTarget>,
    ) {
        for line in hanging_spans(gutter, spans, width) {
            self.push(line, target);
        }
    }

    fn push_dim(&mut self, text: &str) {
        self.push(
            Line::from(Span::styled(
                escape_terminal_controls(text),
                theme::current().tool_dim,
            )),
            None,
        );
    }

    fn push_notices(&mut self, notices: &[String]) {
        for notice in notices {
            self.push_dim(notice);
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
            let target = clickable(note.origin.path(), index);
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
                        clickable(note.origin.path(), index),
                    );
                    drawn += 1;
                }
                index += 1;
            }
        }
        self.finish(hidden);
    }

    /// The heading stays, and the lines are what a budget takes, counted as
    /// the model reads them rather than as they wrap.
    fn push_lines(&mut self, heading: &str, lines: &[MemoryLine], budget: usize, width: u16) {
        if lines.is_empty() {
            return;
        }
        self.push_dim(heading);
        let (room, hidden) = body_window(lines.len(), budget.saturating_sub(self.lines.len()));
        let shown = &lines[..room];
        let gutter = Gutter::new(shown.iter().map(MemoryLine::address));
        let theme = theme::current();
        for line in shown {
            let style = match line.pending {
                true => theme.tool_dim,
                false => theme.tool,
            };
            self.push_hung(
                gutter.address(&line.address()),
                Vec::from([Span::styled(escape_terminal_controls(&line.text), style)]),
                width,
                None,
            );
        }
        self.finish(hidden);
    }

    /// Like a read: every hit's own row is pinned, and the matching lines
    /// under them are what a budget takes away.
    fn push_hits(
        &mut self,
        heading: &str,
        query: &str,
        hits: &[MemoryHit],
        budget: usize,
        width: u16,
    ) {
        if hits.is_empty() {
            return;
        }
        self.push_dim(heading);
        let excerpts = hits.iter().filter(|hit| hit.line.is_some()).count();
        let pinned = self.lines.len() + hits.len();
        let (mut room, hidden) = body_window(excerpts, budget.saturating_sub(pinned));
        let gutter = Gutter::new(hits.iter().map(MemoryHit::address));
        let terms = terms(query);
        let theme = theme::current();
        for (index, hit) in hits.iter().enumerate() {
            let target = clickable(hit.path.as_deref(), index);
            let mut spans = marked(&hit.name, theme.tool_path, &terms);
            if !hit.heading.is_empty() {
                spans.push(Span::styled(
                    format!("{GAP}{}", escape_terminal_controls(&hit.heading)),
                    theme.tool,
                ));
            }
            self.push_hung(gutter.address(&hit.address()), spans, width, target);
            let Some(line) = hit.line.as_deref().filter(|_| room > 0) else {
                continue;
            };
            room -= 1;
            self.push_hung(
                gutter.blank(),
                marked(line, theme.tool_dim, &terms),
                width,
                target,
            );
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

/// The column the addresses of one card stand in, right-aligned so the text
/// beside them starts in one column, as line numbers keep code aligned.
struct Gutter {
    width: usize,
}

impl Gutter {
    fn new(addresses: impl Iterator<Item = String>) -> Self {
        Self {
            width: addresses
                .map(|address| address.width())
                .max()
                .unwrap_or_default(),
        }
    }

    fn address(&self, address: &str) -> Span<'static> {
        let width = self.width;
        Span::styled(
            format!("{address:>width$}{ADDRESS_GAP}"),
            theme::current().diff_line_nr,
        )
    }

    fn blank(&self) -> Span<'static> {
        Span::raw(" ".repeat(self.width + ADDRESS_GAP.len()))
    }
}

/// `text` in `style`, with the searched words in it marked the way a search
/// marks its matches.
fn marked(text: &str, style: Style, terms: &[String]) -> Vec<Span<'static>> {
    let text = escape_terminal_controls(text);
    let ranges = term_ranges(&text, terms);
    highlight(
        Vec::from([Span::styled(text, style)]),
        &ranges,
        theme::current().item_match,
    )
}

/// Every row of a note answers for it, the way a batch child's rows do: an open
/// note is one thing to click, not a header with unaddressed text beneath it.
fn clickable(path: Option<&str>, index: usize) -> Option<RowTarget> {
    path.map(|_| RowTarget::Item(index))
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
    use caudra_agent::{MemoryHit, MemoryLine, MemoryOrigin, MemoryOutput};
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
    const VIEW_HEADING: &str = "4 entries in 3 lines, oldest first:";
    const QUERY: &str = "flaky";
    const HIDDEN_NOTICE: &str =
        "The 3 oldest entries are left out until their summaries are written; search finds them.";
    const PINNED_MSG: &str = "a note's own row is never what a budget takes";
    const CLICK_MSG: &str = "a local note takes the click, a remote one cannot";
    const MODEL_TEXT_MSG: &str = "the card draws every word the model read";
    const HANG_MSG: &str = "a wrapped row starts under the text, never under the address";
    const ALIGN_MSG: &str = "right-aligned addresses start every text in one column";
    const DIM_MSG: &str = "the heading and a line not summarized yet are dimmed";
    const MARK_MSG: &str = "the searched words are marked, whatever their case";
    const COLLAPSE_MSG: &str = "a hit's own row is never what a budget takes";
    const NARROW: u16 = 40;
    const LONG_TEXT: &str =
        "a summary line long enough that a narrow card has to break it over several rows";
    const EXCERPT: &str = "Retry the FLAKY suite once.";
    const MARKED_WORD: &str = "FLAKY";

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

    fn view(notices: Vec<String>) -> MemoryOutput {
        MemoryOutput::Lines {
            heading: VIEW_HEADING.to_owned(),
            lines: Vec::from([
                MemoryLine {
                    id: 0,
                    count: 2,
                    text: format!("{NOTE}: first. {OTHER}: second"),
                    pending: false,
                },
                MemoryLine {
                    id: 2,
                    count: 1,
                    text: format!("note {NOTE} first, rewritten"),
                    pending: false,
                },
                MemoryLine {
                    id: 3,
                    count: 1,
                    text: format!("(not summarized yet) note {OTHER}: Architecture"),
                    pending: true,
                },
            ]),
            notices,
        }
    }

    fn hit(seq: u64, name: &str, path: Option<String>) -> MemoryHit {
        MemoryHit {
            seq,
            name: name.to_owned(),
            heading: "Gotchas".to_owned(),
            line: Some(EXCERPT.to_owned()),
            path,
        }
    }

    fn hits(hits: Vec<MemoryHit>) -> MemoryOutput {
        MemoryOutput::Hits {
            query: QUERY.to_owned(),
            hits,
            notices: Vec::new(),
        }
    }

    fn search() -> MemoryOutput {
        hits(Vec::from([
            hit(4, NOTE, Some(format!("/notes/{NOTE}"))),
            hit(120, OTHER, None),
        ]))
    }

    fn lines(lines: &[(u64, u64, &str)]) -> MemoryOutput {
        MemoryOutput::Lines {
            heading: VIEW_HEADING.to_owned(),
            lines: lines
                .iter()
                .map(|&(id, count, text)| MemoryLine {
                    id,
                    count,
                    text: text.to_owned(),
                    pending: false,
                })
                .collect(),
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

    fn rendered_at(output: &MemoryOutput, width: u16) -> Vec<String> {
        render(output, usize::MAX, width)
            .0
            .iter()
            .map(text)
            .collect()
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

    #[test_case(file_note(NOTE), Some(RowTarget::Item(0)) ; "a_note_on_this_host")]
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
            note_path(&output, RowTarget::Item(1)),
            Some(PathBuf::from(format!("/notes/{OTHER}")))
        );
        assert_eq!(note_path(&output, RowTarget::Item(9)), None, "{CLICK_MSG}");
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
            note_path(&output, RowTarget::Item(1)),
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

    fn words(text: &str) -> Vec<&str> {
        text.split(|c: char| c.is_whitespace() || c == '|')
            .map(|word| word.trim_end_matches(':'))
            .filter(|word| !word.is_empty() && *word != "-")
            .collect()
    }

    /// The card is laid out differently from the model's text, but holds every
    /// word of it, in the same order.
    #[test_case(view(Vec::from([HIDDEN_NOTICE.to_owned()])) ; "a_view_with_its_notice")]
    #[test_case(view(Vec::new()) ; "a_view")]
    #[test_case(search() ; "a_search")]
    fn the_card_draws_every_word_the_model_read(output: MemoryOutput) {
        let (lines, _, truncated) = render(&output, usize::MAX, WIDTH);
        let drawn: Vec<String> = lines.iter().map(text).collect();
        let drawn = drawn.join("\n");
        let model = output.as_display_text();

        assert!(!truncated);
        assert_eq!(words(&drawn), words(&model), "{MODEL_TEXT_MSG}");
    }

    #[test]
    fn a_wrapped_line_hangs_under_its_text() {
        let output = lines(&[(0, 32, LONG_TEXT), (32, 1, LONG_TEXT)]);
        let drawn = rendered_at(&output, NARROW);
        let address = "0+32 ";

        assert!(drawn[1].starts_with(address), "{drawn:?}");
        let continued: Vec<&String> = drawn[2..]
            .iter()
            .filter(|row| !row.trim_start().starts_with("32+1"))
            .collect();
        assert!(!continued.is_empty(), "{drawn:?}");
        for row in continued {
            assert!(
                row.starts_with(&" ".repeat(address.len())),
                "{HANG_MSG}: {drawn:?}"
            );
            assert!(
                !row[address.len()..].starts_with(' '),
                "{HANG_MSG}: {drawn:?}"
            );
        }
    }

    #[test]
    fn addresses_right_align_so_the_text_lines_up() {
        let output = lines(&[(0, 32, OTHER), (624, 1, NOTE)]);
        let drawn = rendered(&output, usize::MAX);

        assert_eq!(drawn[1], format!(" 0+32 {OTHER}"), "{ALIGN_MSG}");
        assert_eq!(drawn[2], format!("624+1 {NOTE}"), "{ALIGN_MSG}");
        let drawn = rendered(&lines(&[(0, 1, OTHER), (624, 1, NOTE)]), usize::MAX);
        assert_eq!(drawn[1].find(OTHER), drawn[2].find(NOTE), "{ALIGN_MSG}");
    }

    #[test]
    fn the_heading_and_a_pending_line_are_dimmed() {
        let theme = theme::current();
        let (lines, _, _) = render(&view(Vec::new()), usize::MAX, WIDTH);
        let text_style = |line: &Line<'static>| line.spans.last().unwrap().style;

        assert_eq!(lines[0].spans[0].style, theme.tool_dim, "{DIM_MSG}");
        assert_eq!(text_style(&lines[1]), theme.tool, "{DIM_MSG}");
        assert_eq!(text_style(&lines[3]), theme.tool_dim, "{DIM_MSG}");
        assert_eq!(lines[1].spans[0].style, theme.diff_line_nr);
    }

    #[test]
    fn a_hit_marks_the_searched_words() {
        let theme = theme::current();
        let (lines, _, _) = render(&search(), usize::MAX, WIDTH);
        let excerpt = lines
            .iter()
            .find(|line| text(line).contains(EXCERPT))
            .unwrap();

        let marked: Vec<&str> = excerpt
            .spans
            .iter()
            .filter(|span| span.style == theme.tool_dim.patch(theme.item_match))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(marked, [MARKED_WORD], "{MARK_MSG}");
    }

    #[test]
    fn a_local_hit_takes_the_click_and_a_remote_one_cannot() {
        let output = search();
        let (lines, rows, _) = render(&output, usize::MAX, WIDTH);
        let clicked: Vec<(String, Option<RowTarget>)> = lines
            .iter()
            .map(text)
            .zip(rows)
            .filter(|(_, row)| row.is_some())
            .collect();

        assert_eq!(clicked.len(), 2, "{CLICK_MSG}: {clicked:?}");
        assert!(
            clicked
                .iter()
                .all(|(row, target)| !row.contains(OTHER) && *target == Some(RowTarget::Item(0))),
            "{CLICK_MSG}: {clicked:?}"
        );
        assert_eq!(
            note_path(&output, RowTarget::Item(0)),
            Some(PathBuf::from(format!("/notes/{NOTE}")))
        );
        assert_eq!(note_path(&output, RowTarget::Item(1)), None, "{CLICK_MSG}");
    }

    #[test]
    fn a_collapsed_search_keeps_every_hit_row_and_drops_excerpts() {
        let output = search();
        let (lines, _, truncated) = render(&output, 3, WIDTH);
        let drawn: Vec<String> = lines.iter().map(text).collect();

        assert!(truncated);
        assert!(
            drawn.iter().any(|row| row.contains(NOTE)),
            "{COLLAPSE_MSG}: {drawn:?}"
        );
        assert!(
            drawn.iter().any(|row| row.contains(OTHER)),
            "{COLLAPSE_MSG}: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|row| row.contains(EXCERPT)),
            "{COLLAPSE_MSG}: {drawn:?}"
        );
    }

    #[test]
    fn a_collapsed_view_keeps_its_heading_and_counts_what_it_holds_back() {
        let (lines, _, truncated) = render(&view(Vec::new()), 2, WIDTH);

        assert!(truncated);
        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[0]), VIEW_HEADING);
    }
}
