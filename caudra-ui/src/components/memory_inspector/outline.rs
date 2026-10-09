//! The outline: the view's lines oldest first, as the model reads them, each
//! opening into the two lines it was made from, down to the entries.

use std::borrow::Cow;
use std::collections::HashSet;

use caudra_agent::memory::search::Hit;
use caudra_agent::memory::snapshot::{EntryStatus, MemorySnapshot};
use caudra_agent::memory::tree::{Block, Part};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::components::escape_terminal_controls;
use crate::components::tool_display::{TREE_BRANCH, TREE_GAP, TREE_LAST, TREE_TRUNK};
use crate::theme;

pub(super) const OPEN_CHEVRON: &str = "\u{25be} ";
pub(super) const SHUT_CHEVRON: &str = "\u{25b8} ";
pub(super) const NO_CHEVRON: &str = "  ";
pub(super) const LEVEL_CELL: &str = "\u{2588}";
const SUMMARIZED: &str = "\u{25cf}";
const VERBATIM: &str = "\u{25cb}";
const UNBUILT: &str = "\u{25cc}";
const SUPERSEDED: &str = "\u{2192} ";
const REMINDER: &str = "reminder ";
const WRITTEN_HERE: &str = "written here ";
const HIT_SEPARATOR: &str = " \u{b7} ";
const COLUMN_GAP: &str = " ";
const ELLIPSIS: char = '\u{2026}';
const LINE_BREAKS: [char; 2] = ['\n', '\r'];

/// One row of the outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) part: Part,
    /// The tree glyphs in front of the row, one per ancestor below its root.
    pub(super) guides: String,
    pub(super) kind: RowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowKind {
    /// A line of the view, or a node below one.
    Node,
    /// An entry made after the session's view was taken.
    Tail(TailMark),
    /// A search hit, by its place among the hits.
    Hit(usize),
}

/// How an entry after the session's view reached the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TailMark {
    /// Another session wrote it, so a `# Memory updated` reminder listed it.
    Reminder,
    /// This session wrote it.
    WrittenHere,
}

impl Row {
    pub(super) fn has_children(&self) -> bool {
        self.kind == RowKind::Node && self.part.level > 0
    }

    /// Columns from the row's left edge to its chevron.
    pub(super) fn chevron_column(&self) -> u16 {
        u16::try_from(self.guides.width()).unwrap_or(u16::MAX)
    }
}

/// Every root, and below each expanded row its two children.
pub(super) fn tree_rows(roots: &[Part], expanded: &HashSet<Part>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(roots.len());
    for root in roots {
        push_node(&mut rows, root.clone(), String::new(), "", expanded);
    }
    rows
}

fn push_node(
    rows: &mut Vec<Row>,
    part: Part,
    guides: String,
    indent: &str,
    expanded: &HashSet<Part>,
) {
    let children = part.children().filter(|_| expanded.contains(&part));
    rows.push(Row {
        part,
        guides,
        kind: RowKind::Node,
    });
    for (index, child) in children.into_iter().flatten().enumerate() {
        let (glyph, trunk) = match index {
            0 => (TREE_BRANCH, TREE_TRUNK),
            _ => (TREE_LAST, TREE_GAP),
        };
        let below = format!("{indent}{trunk}");
        push_node(rows, child, format!("{indent}{glyph}"), &below, expanded);
    }
}

/// The widths every row pads to, so the staircase of level bars and the
/// texts after them start in one column at every depth.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Columns {
    /// The tree guides and the address together.
    lead: usize,
    bar: usize,
}

impl Columns {
    pub(super) fn of(rows: &[Row]) -> Self {
        rows.iter().fold(Self::default(), |columns, row| Self {
            lead: columns
                .lead
                .max(row.guides.width() + row.part.to_string().len()),
            bar: columns.bar.max(bar_cells(&row.part)),
        })
    }

    fn address(&self, row: &Row) -> usize {
        self.lead.saturating_sub(row.guides.width())
    }
}

/// What every row of one frame is drawn against.
pub(super) struct Look<'a> {
    pub(super) snapshot: &'a MemorySnapshot,
    pub(super) hits: &'a [Hit],
    /// The session's view, whose lines read as the prompt holds them.
    pub(super) frozen: Option<&'a Block>,
    pub(super) columns: Columns,
    pub(super) now_ms: i64,
    pub(super) spinner: &'static str,
    pub(super) width: usize,
}

/// `▸ 368+8  ████ ● automation-catalog: discovery order…`, cut to the width.
pub(super) fn row_line(
    row: &Row,
    look: &Look<'_>,
    expanded: bool,
    selected: bool,
) -> Line<'static> {
    let t = theme::current();
    let snapshot = look.snapshot;
    let part = &row.part;
    let chevron = match (row.has_children(), expanded) {
        (false, _) => NO_CHEVRON,
        (true, true) => OPEN_CHEVRON,
        (true, false) => SHUT_CHEVRON,
    };
    let (glyph, glyph_style) = status_glyph(snapshot, part, look.now_ms, look.spinner);
    let superseded = match (row.kind, leaf_status(snapshot, part)) {
        (
            RowKind::Node | RowKind::Hit(_),
            Some(EntryStatus::Rewritten(seq) | EntryStatus::Deleted(seq)),
        ) => Some(Some(seq)),
        (RowKind::Node | RowKind::Hit(_), Some(EntryStatus::Forgotten)) => Some(None),
        _ => None,
    };
    let (address_style, text_style) = match (selected, superseded.is_some()) {
        (true, _) => (t.item_selected, t.item_selected),
        (false, true) => (t.tool_dim, t.tool_dim),
        (false, false) => (t.item, t.item),
    };
    let mut spans = vec![
        Span::styled(row.guides.clone(), t.tool_dim),
        Span::styled(chevron, t.tool_dim),
        Span::styled(
            format!(
                "{:<width$}",
                part.to_string(),
                width = look.columns.address(row)
            ),
            address_style,
        ),
        Span::raw(COLUMN_GAP),
        Span::styled(
            format!(
                "{:<width$}",
                LEVEL_CELL.repeat(bar_cells(part)),
                width = look.columns.bar
            ),
            t.item_desc,
        ),
        Span::raw(COLUMN_GAP),
        Span::styled(glyph, glyph_style),
        Span::raw(COLUMN_GAP),
    ];
    match (row.kind, superseded) {
        (RowKind::Tail(TailMark::Reminder), _) => spans.push(Span::styled(REMINDER, t.accent)),
        (RowKind::Tail(TailMark::WrittenHere), _) => {
            spans.push(Span::styled(WRITTEN_HERE, t.tool_success))
        }
        (_, Some(Some(seq))) => spans.push(Span::styled(format!("{SUPERSEDED}{seq} "), t.tool_dim)),
        _ => {}
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let text = match row.kind {
        RowKind::Hit(index) => look.hits.get(index).map(hit_text).unwrap_or_default(),
        RowKind::Node | RowKind::Tail(_) => one_line(&line_text(snapshot, look.frozen, row)),
    };
    spans.push(Span::styled(
        cut(&text, look.width.saturating_sub(used)),
        text_style,
    ));
    Line::from(spans)
}

/// `●` summarized, `○` kept word for word, `◌` not summarized yet, or the
/// spinner while a summarizer holds the node.
pub(super) fn status_glyph(
    snapshot: &MemorySnapshot,
    part: &Part,
    now_ms: i64,
    spinner: &'static str,
) -> (&'static str, Style) {
    let t = theme::current();
    if snapshot.leased(part, now_ms) {
        return (spinner, t.spinner);
    }
    match snapshot.tree.built(part) {
        Some(built) if built.verbatim => (VERBATIM, t.tool_dim),
        Some(_) => (SUMMARIZED, t.accent),
        None => (UNBUILT, t.tool_dim),
    }
}

/// What became of the entry a leaf stands for. `None` above the leaves.
pub(super) fn leaf_status(snapshot: &MemorySnapshot, part: &Part) -> Option<EntryStatus> {
    if part.level > 0 {
        return None;
    }
    snapshot
        .statuses
        .get(usize::try_from(part.index).ok()?)
        .copied()
}

/// What a row's line says: a line of the session's view as its prompt holds
/// it, which may predate the summary written since, else the tree's line.
pub(super) fn line_text<'a>(
    snapshot: &'a MemorySnapshot,
    frozen: Option<&'a Block>,
    row: &Row,
) -> Cow<'a, str> {
    let held = frozen
        .filter(|_| row.kind == RowKind::Node && row.guides.is_empty())
        .and_then(|block| block.lines.iter().find(|line| line.part == row.part));
    match held {
        Some(line) => Cow::Borrowed(&line.text),
        None => snapshot.tree.line(&row.part),
    }
}

/// A line as one row: newlines as spaces, control characters escaped.
pub(super) fn one_line(text: &str) -> String {
    escape_terminal_controls(&text.replace(LINE_BREAKS, " "))
}

fn hit_text(hit: &Hit) -> String {
    let detail = hit.line.as_deref().unwrap_or(&hit.heading);
    match detail.is_empty() {
        true => one_line(&hit.name),
        false => one_line(&format!("{}{HIT_SEPARATOR}{detail}", hit.name)),
    }
}

/// The level bar: one cell per doubling, so older lines covering more
/// entries stand taller.
fn bar_cells(part: &Part) -> usize {
    usize::try_from(part.level)
        .unwrap_or(usize::MAX)
        .saturating_add(1)
}

/// `text` in at most `columns` columns, ending in `…` when it had to be cut.
pub(super) fn cut(text: &str, columns: usize) -> String {
    if text.width() <= columns {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width + 1 > columns {
            break;
        }
        used += width;
        kept.push(character);
    }
    if columns > 0 {
        kept.push(ELLIPSIS);
    }
    kept
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const LONG: &str = "abcdefghij";

    fn part(level: u32, index: u64) -> Part {
        Part { level, index }
    }

    #[test]
    fn an_expanded_row_lists_its_children_under_tree_glyphs() {
        let root = part(2, 0);
        let left = part(1, 0);
        let expanded: HashSet<Part> = [root.clone(), left.clone()].into();

        let rows = tree_rows(std::slice::from_ref(&root), &expanded);

        let drawn: Vec<(String, String)> = rows
            .iter()
            .map(|row| (row.part.to_string(), row.guides.clone()))
            .collect();
        assert_eq!(
            drawn,
            [
                ("0+4".to_owned(), String::new()),
                ("0+2".to_owned(), TREE_BRANCH.to_owned()),
                ("0+1".to_owned(), format!("{TREE_TRUNK}{TREE_BRANCH}")),
                ("1+1".to_owned(), format!("{TREE_TRUNK}{TREE_LAST}")),
                ("2+2".to_owned(), TREE_LAST.to_owned()),
            ]
        );
    }

    #[test_case(LONG, 10, LONG ; "fits")]
    #[test_case(LONG, 4, "abc\u{2026}" ; "cut")]
    #[test_case(LONG, 0, "" ; "no_room")]
    fn cut_keeps_what_fits(text: &str, columns: usize, expected: &str) {
        assert_eq!(cut(text, columns), expected);
    }

    #[test]
    fn one_line_escapes_controls_and_joins_lines() {
        assert_eq!(one_line("a\nb\u{1b}[2J"), "a b\\u{1b}[2J");
    }
}
