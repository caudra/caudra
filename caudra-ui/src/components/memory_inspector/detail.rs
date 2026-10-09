//! The detail pane's words and its drawing: where the selected node stands in
//! the mechanism, who wrote the entry under it, and the few nodes around it.

use std::fmt::Write;

use caudra_agent::memory::snapshot::{EntryStatus, MemorySnapshot, Placement};
use caudra_agent::memory::tree::{NODE, Part};
use caudra_storage::memory_journal::{EntryKind, EntryMeta, EntryOrigin};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::text::{Line, Span};

use super::MemoryMode;
use crate::components::tool_display::{TREE_BRANCH, TREE_GAP, TREE_LAST, TREE_TRUNK};
use crate::theme;

/// Levels the drawing shows below its top node, so it holds at most seven.
const DEPTH: u32 = 2;
const FLOWCHART: &str = "flowchart TD";
const LABEL_BREAK: &str = "<br/>";
const SOLID_EDGE: &str = "-->";
const DOTTED_EDGE: &str = "-.->";
const NODE_PREFIX: &str = "N";
const SELECTED_SHAPE: (&str, &str) = ("{", "}");
const VIEW_SHAPE: (&str, &str) = ("[[", "]]");
const BUILT_SHAPE: (&str, &str) = ("[", "]");
const PENDING_SHAPE: (&str, &str) = ("(", ")");
const SELECTED_OPEN: &str = "\u{2039}";
const SELECTED_CLOSE: &str = "\u{203a}";
const PENDING_STATE: &str = "pending";
const LEASED_STATE: &str = "summarizing";
const VIEW_STATE: &str = "view";
const DAY_FORMAT: &str = "%b %-d";
const MINUTE_FORMAT: &str = "%b %-d %H:%M";
const SEPARATOR: &str = " \u{b7} ";
const SPAN_ARROW: &str = " \u{2192} ";
const RANGE_DASH: &str = "\u{2013}";
const CRUMB: &str = " \u{203a} ";
const VERBATIM: &str = "verbatim";
const SUMMARIZED: &str = "summarized";
const UNBUILT: &str = "not summarized yet";
const IN_SESSION_VIEW: &str = "In this session's view. Live: ";
const NOTE_KIND: &str = "note";
const DELETE_KIND: &str = "deletion of";
const WRITTEN: &str = "written";
const RECORDED: &str = "recorded";
const THIS_SESSION: &str = "this session";
const ANOTHER_SESSION: &str = "another session";
const EXTERNAL_EDIT: &str = "an edit made outside Caudra";
const IMPORT: &str = "the import";

/// What the pane is drawn against.
pub(super) struct Scene<'a> {
    pub(super) snapshot: &'a MemorySnapshot,
    /// The view lines of the mode on screen, oldest first.
    pub(super) roots: &'a [Part],
    pub(super) mode: MemoryMode,
    pub(super) now_ms: i64,
}

impl Scene<'_> {
    /// The second line of a node's label: what it waits for, or what it is.
    fn state(&self, part: &Part) -> Option<String> {
        let entries = self.snapshot.tree.entries();
        if !part.formed(entries) {
            return Some(format!("needs {} more", part.end() - entries));
        }
        if self.snapshot.leased(part, self.now_ms) {
            return Some(LEASED_STATE.to_owned());
        }
        if !self.snapshot.tree.is_built(part) {
            return Some(PENDING_STATE.to_owned());
        }
        if !self.roots.contains(part) {
            return None;
        }
        Some(match (self.mode, self.snapshot.placement(part)) {
            (MemoryMode::Live, Some(Placement::Merges { ahead, .. })) => {
                format!("{} to merge", ordinal(ahead + 1))
            }
            _ => VIEW_STATE.to_owned(),
        })
    }

    /// A node's text exists, so the line it stands for reads whole.
    fn solid(&self, part: &Part) -> bool {
        part.formed(self.snapshot.tree.entries()) && self.snapshot.tree.is_built(part)
    }
}

/// The nodes the drawing shows, top first: the selection's parent and the two
/// levels below it, or its grandparent for a leaf, without the nodes that
/// start past the last entry.
fn neighbourhood(selected: &Part, entries: u64) -> Vec<Part> {
    let level = (selected.level + 1).max(DEPTH);
    let top = Part {
        level,
        index: selected.index >> (level - selected.level),
    };
    let mut parts = vec![top.clone()];
    let mut layer = vec![top];
    for _ in 0..DEPTH {
        layer = layer
            .iter()
            .filter_map(Part::children)
            .flatten()
            .filter(|part| part.start() < entries)
            .collect();
        parts.extend(layer.iter().cloned());
    }
    parts
}

fn drawn_children<'a>(part: &Part, parts: &'a [Part]) -> impl Iterator<Item = &'a Part> {
    let children = part.children();
    parts.iter().filter(move |candidate| {
        children
            .as_ref()
            .is_some_and(|pair| pair.contains(candidate))
    })
}

/// The neighbourhood as a mermaid flowchart. A view line of the mode has a
/// double border, a node with text a square one, a node without text round
/// corners and a dotted edge in, and the selection is a rhombus.
pub(super) fn diagram_source(scene: &Scene<'_>, selected: &Part) -> String {
    let parts = neighbourhood(selected, scene.snapshot.tree.entries());
    let mut source = String::from(FLOWCHART);
    for part in &parts {
        let (open, close) = match () {
            () if part == selected => SELECTED_SHAPE,
            () if scene.roots.contains(part) => VIEW_SHAPE,
            () if scene.solid(part) => BUILT_SHAPE,
            () => PENDING_SHAPE,
        };
        let _ = write!(source, "\n  {}{open}{part}", node_id(part));
        if let Some(state) = scene.state(part) {
            let _ = write!(source, "{LABEL_BREAK}{state}");
        }
        source.push_str(close);
    }
    for part in &parts {
        for child in drawn_children(part, &parts) {
            let edge = match scene.solid(child) {
                true => SOLID_EDGE,
                false => DOTTED_EDGE,
            };
            let _ = write!(source, "\n  {} {edge} {}", node_id(part), node_id(child));
        }
    }
    source
}

fn node_id(part: &Part) -> String {
    format!("{NODE_PREFIX}{}_{}", part.start(), part.count())
}

/// The neighbourhood drawn with the outline's tree glyphs, for a terminal
/// that draws no diagrams.
pub(super) fn diagram_tree(scene: &Scene<'_>, selected: &Part) -> Vec<Line<'static>> {
    let parts = neighbourhood(selected, scene.snapshot.tree.entries());
    let mut lines = Vec::with_capacity(parts.len());
    push_tree(
        &mut lines,
        scene,
        selected,
        &parts,
        &parts[0],
        String::new(),
        "",
    );
    lines
}

fn push_tree(
    lines: &mut Vec<Line<'static>>,
    scene: &Scene<'_>,
    selected: &Part,
    parts: &[Part],
    part: &Part,
    guides: String,
    indent: &str,
) {
    let t = theme::current();
    let address = match (part == selected, scene.solid(part)) {
        (true, _) => Span::styled(
            format!("{SELECTED_OPEN}{part}{SELECTED_CLOSE}"),
            t.item_selected,
        ),
        (false, true) => Span::styled(part.to_string(), t.item),
        (false, false) => Span::styled(part.to_string(), t.tool_dim),
    };
    let mut spans = vec![Span::styled(guides, t.tool_dim), address];
    if let Some(state) = scene.state(part) {
        spans.push(Span::styled(format!(" {state}"), t.tool_dim));
    }
    lines.push(Line::from(spans));
    let children: Vec<&Part> = drawn_children(part, parts).collect();
    for (index, child) in children.iter().enumerate() {
        let (glyph, trunk) = match index + 1 == children.len() {
            true => (TREE_LAST, TREE_GAP),
            false => (TREE_BRANCH, TREE_TRUNK),
        };
        let below = format!("{indent}{trunk}");
        push_tree(
            lines,
            scene,
            selected,
            parts,
            child,
            format!("{indent}{glyph}"),
            &below,
        );
    }
}

/// `368+8 · entries 368–375 · Sep 27 → Sep 28 · 498 of 512 B · haiku`.
pub(super) fn heading(snapshot: &MemorySnapshot, part: &Part) -> Line<'static> {
    let t = theme::current();
    let last = part
        .end()
        .min(snapshot.tree.entries())
        .saturating_sub(1)
        .max(part.start());
    let mut text = match part.level {
        0 => format!("entry {}", part.start()),
        _ => format!("entries {}{RANGE_DASH}{last}", part.start()),
    };
    if let Some(dates) = date_span(snapshot, part.start(), last, part.level == 0) {
        let _ = write!(text, "{SEPARATOR}{dates}");
    }
    text.push_str(SEPARATOR);
    match snapshot.tree.built(part) {
        Some(built) => {
            let author = match built.verbatim {
                true => VERBATIM,
                false => snapshot
                    .node(part)
                    .and_then(|node| node.model.as_deref())
                    .unwrap_or(SUMMARIZED),
            };
            let _ = write!(text, "{} of {NODE} B{SEPARATOR}{author}", built.text.len());
        }
        None => text.push_str(UNBUILT),
    }
    Line::from(vec![
        Span::styled(part.to_string(), t.accent),
        Span::styled(SEPARATOR, t.tool_dim),
        Span::styled(text, t.item_desc),
    ])
}

/// When the first and the last entry were made, one date when they share it.
fn date_span(snapshot: &MemorySnapshot, first: u64, last: u64, leaf: bool) -> Option<String> {
    let created = |seq: u64| {
        let meta = snapshot.entries.get(usize::try_from(seq).ok()?)?;
        Some(meta.created_ms)
    };
    let format = match leaf {
        true => MINUTE_FORMAT,
        false => DAY_FORMAT,
    };
    let from = local_time(created(first)?, format)?;
    let to = local_time(created(last)?, format)?;
    Some(match from == to {
        true => from,
        false => format!("{from}{SPAN_ARROW}{to}"),
    })
}

fn local_time(ms: i64, format: &str) -> Option<String> {
    let at = Timestamp::from_millisecond(ms).ok()?;
    Some(at.to_zoned(TimeZone::system()).strftime(format).to_string())
}

/// One sentence placing the node in the mechanism, against the live view.
pub(super) fn placement(scene: &Scene<'_>, part: &Part) -> Option<String> {
    let sentence = match scene.snapshot.placement(part)? {
        Placement::Merges {
            sibling,
            parent,
            ahead,
        } => format!(
            "Merges with {sibling} into {parent} when the view needs room: {} in line.",
            ordinal(ahead + 1)
        ),
        Placement::AwaitsSummary { parent } => {
            format!("Waits for {parent} to be summarized before it can merge.")
        }
        Placement::AwaitsSibling { sibling } => {
            format!("Waits until the lines of {sibling} merge into it.")
        }
        Placement::AwaitsEntries { count } => format!(
            "Waits for {count} more {} before it can merge.",
            entries_word(count)
        ),
        Placement::Inside { line } => format!(
            "Inside {line}. The agent opens it with zoom({}, {}).",
            part.start(),
            part.count()
        ),
    };
    Some(match scene.mode {
        MemoryMode::Session if scene.roots.contains(part) => format!("{IN_SESSION_VIEW}{sentence}"),
        _ => sentence,
    })
}

/// `note flaky-tests.md · written Sep 28 14:02 by this session`.
pub(super) fn authorship(meta: &EntryMeta, session_id: &str) -> String {
    let (kind, made) = match meta.kind {
        EntryKind::Note => (NOTE_KIND, WRITTEN),
        EntryKind::Delete => (DELETE_KIND, RECORDED),
    };
    let author = match &meta.origin {
        EntryOrigin::Session(id) if id == session_id => THIS_SESSION,
        EntryOrigin::Session(_) => ANOTHER_SESSION,
        EntryOrigin::External => EXTERNAL_EDIT,
        EntryOrigin::Import => IMPORT,
    };
    let when = local_time(meta.created_ms, MINUTE_FORMAT).unwrap_or_default();
    format!("{kind} {}{SEPARATOR}{made} {when} by {author}", meta.name)
}

pub(super) fn status_sentence(status: EntryStatus) -> String {
    match status {
        EntryStatus::Current => "The newest entry of its name.".to_owned(),
        EntryStatus::Rewritten(seq) => format!("Rewritten at entry {seq}."),
        EntryStatus::Deleted(seq) => format!("Deleted at entry {seq}."),
        EntryStatus::Forgotten => "Forgotten: its text was purged.".to_owned(),
    }
}

/// `4 zooms from the view: 368+8 › 372+4 › 374+2 › 375+1`.
pub(super) fn breadcrumb(path: &[Part]) -> String {
    let crumbs: Vec<String> = path.iter().map(Part::to_string).collect();
    let zooms = match path.len() {
        1 => "1 zoom".to_owned(),
        count => format!("{count} zooms"),
    };
    format!("{zooms} from the view: {}", crumbs.join(CRUMB))
}

pub(super) fn entries_word(count: u64) -> &'static str {
    match count {
        1 => "entry",
        _ => "entries",
    }
}

/// `1st`, `2nd`, `11th`, `23rd`.
pub(super) fn ordinal(position: usize) -> String {
    let suffix = match (position % 10, position % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{position}{suffix}")
}

#[cfg(test)]
mod tests {
    use caudra_agent::memory::tree::zoom_path;
    use caudra_markdown::mermaid::{Shape, parse};
    use test_case::test_case;

    use super::*;
    use crate::components::memory_inspector::tests::Fixture;

    const ENTRIES: u64 = 13;
    const MAX_BOXES: usize = 7;
    const NOT_A_FLOWCHART: &str = "the drawing would fall back to a code fence";
    const TOO_MANY_BOXES: &str = "the drawing holds more than seven boxes";

    fn scene_parts(fixture: &MemorySnapshot) -> Vec<Part> {
        fixture.view.clone()
    }

    #[derive(Debug, Clone, Copy)]
    enum Pick {
        ViewLine,
        BelowView,
        PendingLeaf,
        UnformedParent,
    }

    fn pick(snapshot: &MemorySnapshot, pick: Pick) -> Part {
        match pick {
            Pick::ViewLine => snapshot.view[0].clone(),
            Pick::BelowView => zoom_path(&snapshot.view, 0)[1].clone(),
            Pick::PendingLeaf => Part::leaf(snapshot.tree.entries() - 1),
            Pick::UnformedParent => Part::leaf(snapshot.tree.entries() - 1).parent(),
        }
    }

    #[test_case(Pick::ViewLine ; "view_line")]
    #[test_case(Pick::BelowView ; "node_below_the_view")]
    #[test_case(Pick::PendingLeaf ; "pending_leaf")]
    #[test_case(Pick::UnformedParent ; "unformed_parent")]
    fn every_drawing_parses_as_a_flowchart(choice: Pick) {
        let snapshot = Fixture::notes(ENTRIES)
            .summarized()
            .pending_tail()
            .snapshot();
        let roots = scene_parts(&snapshot);
        let selected = pick(&snapshot, choice);
        let scene = Scene {
            snapshot: &snapshot,
            roots: &roots,
            mode: MemoryMode::Live,
            now_ms: 0,
        };

        let source = diagram_source(&scene, &selected);
        let graph = parse(&source).expect(NOT_A_FLOWCHART);

        assert!(graph.nodes.len() <= MAX_BOXES, "{TOO_MANY_BOXES}: {source}");
        let selection = graph
            .nodes
            .iter()
            .find(|node| node.label.first() == Some(&selected.to_string()))
            .expect(NOT_A_FLOWCHART);
        assert_eq!(selection.shape, Shape::Rhombus, "{source}");
    }

    #[test]
    fn an_unformed_parent_says_how_many_entries_it_needs() {
        let snapshot = Fixture::notes(ENTRIES).summarized().snapshot();
        let roots = scene_parts(&snapshot);
        let scene = Scene {
            snapshot: &snapshot,
            roots: &roots,
            mode: MemoryMode::Live,
            now_ms: 0,
        };
        let newest = Part::leaf(ENTRIES - 1);

        let source = diagram_source(&scene, &newest);

        assert!(source.contains("needs 3 more"), "{source}");
        assert!(source.contains(DOTTED_EDGE), "{source}");
    }

    #[test]
    fn the_tree_drawing_holds_the_same_nodes() {
        let snapshot = Fixture::notes(ENTRIES).summarized().snapshot();
        let roots = scene_parts(&snapshot);
        let scene = Scene {
            snapshot: &snapshot,
            roots: &roots,
            mode: MemoryMode::Live,
            now_ms: 0,
        };
        let selected = Part::leaf(5);

        let lines = diagram_tree(&scene, &selected);
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert_eq!(text.len(), neighbourhood(&selected, ENTRIES).len());
        assert!(text[0].starts_with("4+4"), "{text:?}");
        assert!(
            text.iter().any(|line| line.contains("\u{2039}5+1\u{203a}")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.starts_with(TREE_BRANCH)),
            "{text:?}"
        );
    }

    #[test_case(1, "1st")]
    #[test_case(2, "2nd")]
    #[test_case(3, "3rd")]
    #[test_case(4, "4th")]
    #[test_case(11, "11th")]
    #[test_case(12, "12th")]
    #[test_case(22, "22nd")]
    fn ordinals_read_as_spoken(position: usize, expected: &str) {
        assert_eq!(ordinal(position), expected);
    }

    #[test]
    fn the_breadcrumb_counts_one_zoom_per_node() {
        let path = [
            Part { level: 2, index: 1 },
            Part { level: 1, index: 3 },
            Part::leaf(7),
        ];

        assert_eq!(
            breadcrumb(&path),
            "3 zooms from the view: 4+4 \u{203a} 6+2 \u{203a} 7+1"
        );
    }
}
