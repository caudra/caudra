//! Draws a `Layout` onto a grid of characters.
//!
//! Box-drawing glyphs are derived from a per-cell connection mask rather than
//! written literally, so an edge meeting a border produces the right junction
//! and two crossing edges produce a cross, without a special case per pair.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::layout::{Arrow, Layout, PlacedCluster, PlacedNode};
use super::parse::{EdgeStyle, Graph, Node, Shape};

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

const LIGHT: [char; 16] = [
    ' ', '╵', '╷', '│', '╴', '┘', '┐', '┤', '╶', '└', '┌', '├', '─', '┴', '┬', '┼',
];
const HEAVY: [char; 16] = [
    ' ', '╹', '╻', '┃', '╸', '┛', '┓', '┫', '╺', '┗', '┏', '┣', '━', '┻', '┳', '╋',
];
const ROUND_CORNERS: [(u8, char); 4] = [
    (DOWN | RIGHT, '╭'),
    (DOWN | LEFT, '╮'),
    (UP | RIGHT, '╰'),
    (UP | LEFT, '╯'),
];
const DOTTED_VERTICAL: char = '┆';
const DOTTED_HORIZONTAL: char = '┄';
/// Decision nodes keep ordinary rounded corners and take their shape from a
/// pair of caps instead. Diagonal glyphs meet the corner of a cell while `─`
/// runs through the middle, so they never join up.
const DECISION_CAPS: (char, char) = ('‹', '›');
/// A leaning side is drawn with the delimiter it was written with, stepped one
/// column per row so the run of glyphs reads as one diagonal.
const LEAN_RIGHT_STROKE: char = '/';
const LEAN_LEFT_STROKE: char = '\\';
const FLAG: char = '>';
const BLANK: char = ' ';
/// Second column of a double-width glyph. Keeping it as its own cell is what
/// lets the rest of the layout treat one cell as one display column.
pub const CONTINUATION: char = '\0';

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Blank,
    Border,
    Edge,
    Label,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub role: Role,
}

impl Cell {
    fn blank() -> Self {
        Self {
            ch: BLANK,
            role: Role::Blank,
        }
    }
}

/// How a node's border is drawn. Terminal cells cannot distinguish nine
/// mermaid shapes, so they collapse onto four frames that stay legible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Frame {
    Sharp,
    Round,
    Decision,
    Nested,
}

impl Frame {
    fn of(shape: Shape) -> Self {
        match shape {
            Shape::Rect
            | Shape::Cylinder
            | Shape::Asymmetric
            | Shape::LeanRight
            | Shape::LeanLeft
            | Shape::TrapezoidDown
            | Shape::TrapezoidUp => Self::Sharp,
            Shape::Round | Shape::Stadium | Shape::Circle => Self::Round,
            Shape::Rhombus | Shape::Hexagon => Self::Decision,
            Shape::Subroutine => Self::Nested,
        }
    }

    fn rounded(self) -> bool {
        matches!(self, Self::Round | Self::Decision)
    }
}

/// A cell that participates in the line system, tracked as a connection mask
/// so junctions resolve on their own.
#[derive(Clone, Copy, Default)]
struct Line {
    mask: u8,
    heavy: bool,
    dotted: bool,
    round: bool,
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    cells: Vec<Cell>,
    lines: Vec<Option<Line>>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::blank(); width * height],
            lines: vec![None; width * height],
        }
    }

    fn at(&self, x: usize, y: usize) -> Option<usize> {
        (x < self.width && y < self.height).then(|| y * self.width + x)
    }

    fn put(&mut self, x: usize, y: usize, ch: char, role: Role) {
        if let Some(index) = self.at(x, y) {
            self.cells[index] = Cell { ch, role };
            self.lines[index] = None;
        }
    }

    fn connect(&mut self, x: usize, y: usize, mask: u8, style: Line, role: Role) {
        let Some(index) = self.at(x, y) else { return };
        let existing = self.lines[index].unwrap_or(Line { mask: 0, ..style });
        let merged = Line {
            mask: existing.mask | mask,
            heavy: existing.heavy || style.heavy,
            dotted: existing.dotted && style.dotted,
            round: existing.round && style.round,
        };
        self.lines[index] = Some(merged);
        self.cells[index] = Cell {
            ch: glyph(merged),
            role: match self.cells[index].role {
                Role::Border => Role::Border,
                _ => role,
            },
        };
    }

    pub fn rows(&self) -> impl Iterator<Item = &[Cell]> {
        self.cells.chunks(self.width)
    }

    #[cfg(test)]
    fn to_text(&self) -> String {
        self.rows()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.ch)
                    .filter(|&ch| ch != CONTINUATION)
                    .collect::<String>()
            })
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn glyph(line: Line) -> char {
    let corner = ROUND_CORNERS
        .iter()
        .find(|&&(mask, _)| mask == line.mask)
        .map(|&(_, ch)| ch);
    if line.round
        && let Some(ch) = corner
    {
        return ch;
    }
    let table = match line.heavy {
        true => HEAVY,
        false => LIGHT,
    };
    let ch = table[line.mask as usize];
    match (line.dotted, ch) {
        (true, '│') => DOTTED_VERTICAL,
        (true, '─') => DOTTED_HORIZONTAL,
        _ => ch,
    }
}

pub fn paint(graph: &Graph, layout: &Layout) -> Canvas {
    let mut canvas = Canvas::new(layout.width, layout.height);
    for cluster in &layout.clusters {
        draw_cluster(&mut canvas, cluster);
    }
    for placed in &layout.nodes {
        draw_node(&mut canvas, &graph.nodes[placed.node], placed);
    }
    for edge in &layout.edges {
        draw_edge(&mut canvas, edge);
    }
    // Heads go on after every line, because a line says where an edge runs
    // while a head says which way it points. One edge joining a border must
    // not cost another edge its arrow.
    for edge in &layout.edges {
        draw_arrow(&mut canvas, edge);
    }
    // Ornaments carry the node's shape, so they outrank a line crossing the
    // cell they sit on. In a left-to-right chart an edge leaves a decision
    // exactly where its cap goes.
    for placed in &layout.nodes {
        draw_ornaments(&mut canvas, graph.nodes[placed.node].shape, placed);
    }
    // Labels are text, so they outrank every line they cross. Layout has
    // already nudged them clear of the boxes and of each other.
    for edge in &layout.edges {
        if let Some(label) = &edge.label {
            write_text(&mut canvas, label.x, label.y, &label.text, Role::Label);
        }
    }
    // Titles go on last: an edge leaving a subgraph crosses the frame, and a
    // readable name is worth more than the few cells of line it covers.
    for cluster in &layout.clusters {
        let title = &graph.subgraphs[cluster.subgraph].title;
        if !title.is_empty() && cluster.width > title.width() + 3 {
            write_text(&mut canvas, cluster.x + 2, cluster.y, title, Role::Label);
        }
    }
    canvas
}

/// Subgraph frames are drawn dashed so they never read as a node, with the
/// title sitting on the top border.
fn draw_cluster(canvas: &mut Canvas, cluster: &PlacedCluster) {
    let style = Line {
        dotted: true,
        round: true,
        ..Default::default()
    };
    let (right, bottom) = (
        cluster.x + cluster.width - 1,
        cluster.y + cluster.height - 1,
    );
    for x in cluster.x + 1..right {
        canvas.connect(x, cluster.y, LEFT | RIGHT, style, Role::Border);
        canvas.connect(x, bottom, LEFT | RIGHT, style, Role::Border);
    }
    for y in cluster.y + 1..bottom {
        canvas.connect(cluster.x, y, UP | DOWN, style, Role::Border);
        canvas.connect(right, y, UP | DOWN, style, Role::Border);
    }
    canvas.connect(cluster.x, cluster.y, DOWN | RIGHT, style, Role::Border);
    canvas.connect(right, cluster.y, DOWN | LEFT, style, Role::Border);
    canvas.connect(cluster.x, bottom, UP | RIGHT, style, Role::Border);
    canvas.connect(right, bottom, UP | LEFT, style, Role::Border);
}

fn draw_node(canvas: &mut Canvas, node: &Node, placed: &PlacedNode) {
    let frame = Frame::of(node.shape);
    let (right, bottom) = (placed.x + placed.width - 1, placed.y + placed.height - 1);
    let style = Line {
        round: frame.rounded(),
        ..Default::default()
    };

    for row in 0..placed.height {
        let (left, right) = node.shape.sides(placed.width, placed.height, row);
        let (left, right) = (placed.x + left, placed.x + right);
        let y = placed.y + row;
        // The two rules carry the corners. Between them the sides are either
        // upright, or a slant that steps a column per row.
        if y == placed.y || y == bottom {
            for x in left + 1..right {
                canvas.connect(x, y, LEFT | RIGHT, style, Role::Border);
            }
            let (opening, closing) = match y == placed.y {
                true => (DOWN | RIGHT, DOWN | LEFT),
                false => (UP | RIGHT, UP | LEFT),
            };
            canvas.connect(left, y, opening, style, Role::Border);
            canvas.connect(right, y, closing, style, Role::Border);
        } else if let Some((leading, trailing)) = node.shape.lean() {
            canvas.put(left, y, slant(leading), Role::Border);
            canvas.put(right, y, slant(trailing), Role::Border);
        } else {
            canvas.connect(left, y, UP | DOWN, style, Role::Border);
            canvas.connect(right, y, UP | DOWN, style, Role::Border);
        }
    }

    if frame == Frame::Nested {
        for y in placed.y + 1..bottom {
            canvas.connect(placed.x + 1, y, UP | DOWN, style, Role::Border);
            canvas.connect(right - 1, y, UP | DOWN, style, Role::Border);
        }
    }
    let top = 1 + (placed.height.saturating_sub(2 + node.label.len())) / 2;
    for (row, text) in node.label.iter().enumerate() {
        let (left, right) = node.shape.sides(placed.width, placed.height, top + row);
        let indent = right.saturating_sub(left + 1).saturating_sub(text.width()) / 2;
        let x = placed.x + left + 1 + indent;
        write_text(canvas, x, placed.y + top + row, text, Role::Label);
    }
}

/// The stroke a border draws when it travels `step` columns per row.
fn slant(step: isize) -> char {
    match step < 0 {
        true => LEAN_RIGHT_STROKE,
        false => LEAN_LEFT_STROKE,
    }
}

/// The glyphs that carry a node's shape rather than its outline.
fn draw_ornaments(canvas: &mut Canvas, shape: Shape, placed: &PlacedNode) {
    let (right, bottom) = (placed.x + placed.width - 1, placed.y + placed.height - 1);
    match Frame::of(shape) {
        // One pair of caps on the centre row. Repeating them down a tall box
        // reads as a list rather than a shape.
        Frame::Decision => {
            let middle = placed.y + placed.height / 2;
            canvas.put(placed.x, middle, DECISION_CAPS.0, Role::Border);
            canvas.put(right, middle, DECISION_CAPS.1, Role::Border);
        }
        _ if shape == Shape::Asymmetric => {
            for y in placed.y + 1..bottom {
                canvas.put(placed.x, y, FLAG, Role::Border);
            }
        }
        _ => {}
    }
}

fn write_text(canvas: &mut Canvas, x: usize, y: usize, text: &str, role: Role) {
    let mut cursor = x;
    for ch in text.chars() {
        canvas.put(cursor, y, ch, role);
        for trailing in 1..ch.width().unwrap_or(1) {
            canvas.put(cursor + trailing, y, CONTINUATION, role);
        }
        cursor += ch.width().unwrap_or(1).max(1);
    }
}

fn draw_edge(canvas: &mut Canvas, edge: &super::layout::PlacedEdge) {
    let style = Line {
        heavy: edge.style == EdgeStyle::Thick,
        dotted: edge.style == EdgeStyle::Dotted,
        ..Default::default()
    };
    for pair in edge.points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        match from.0 == to.0 {
            true => {
                let (lo, hi) = (from.1.min(to.1), from.1.max(to.1));
                for y in lo..=hi {
                    let mask = (u8::from(y > lo) * UP) | (u8::from(y < hi) * DOWN);
                    canvas.connect(from.0, y, mask, style, Role::Edge);
                }
            }
            false => {
                let (lo, hi) = (from.0.min(to.0), from.0.max(to.0));
                for x in lo..=hi {
                    let mask = (u8::from(x > lo) * LEFT) | (u8::from(x < hi) * RIGHT);
                    canvas.connect(x, from.1, mask, style, Role::Edge);
                }
            }
        }
    }
}

fn draw_arrow(canvas: &mut Canvas, edge: &super::layout::PlacedEdge) {
    let Some((x, y, arrow)) = edge.arrow else {
        return;
    };
    let head = match arrow {
        Arrow::Up => '▲',
        Arrow::Down => '▼',
        Arrow::Left => '◀',
        Arrow::Right => '▶',
    };
    canvas.put(x, y, head, Role::Edge);
}

#[cfg(test)]
mod tests {
    use super::super::{layout, parse};
    use super::*;
    use test_case::test_case;

    fn render(source: &str) -> String {
        let graph = parse::parse(source).expect("fixture should parse");
        let layout = layout::layout(&graph);
        paint(&graph, &layout).to_text()
    }

    #[test]
    fn a_single_edge_draws_two_boxes_and_a_connector() {
        let art = render("flowchart TD\n  A[One] --> B[Two]");
        assert!(art.contains("One"), "{art}");
        assert!(art.contains("Two"), "{art}");
        assert!(art.contains('▼'), "{art}");
    }

    #[test]
    fn rounded_shapes_use_rounded_corners() {
        let art = render("flowchart TD\n  A(Round) --> B[Square]");
        assert!(art.contains('╭'), "{art}");
        assert!(art.contains('┌'), "{art}");
    }

    #[test]
    fn a_decision_is_marked_by_caps_not_by_diagonals() {
        let art = render("flowchart TD\n  A{Ok?} --> B[Yes]");
        assert!(art.contains(DECISION_CAPS.0), "{art}");
        assert!(art.contains(DECISION_CAPS.1), "{art}");
        assert!(
            !art.contains('╱') && !art.contains('╲'),
            "diagonals never join a horizontal border: {art}"
        );
    }

    #[test]
    fn a_tall_decision_caps_only_its_middle_row() {
        let art = render("flowchart TD\n  A{\"one<br/>two<br/>three\"} --> B[Go]");
        let capped = art
            .lines()
            .filter(|line| line.contains(DECISION_CAPS.0))
            .count();
        assert_eq!(capped, 1, "{art}");
    }

    #[test]
    fn an_edge_leaving_a_decision_does_not_erase_its_cap() {
        let art = render("flowchart LR\n  A{Ok?} --> B[Yes]");
        assert!(art.contains(DECISION_CAPS.1), "{art}");
    }

    /// A returning edge leaves the same border an incoming edge arrives at,
    /// and its line used to erase the arrow head that was already there.
    #[test]
    fn a_returning_edge_does_not_erase_the_arrow_it_crosses() {
        let art = render("flowchart LR\n A[Run] --> B{Pass?}\n B -->|No| C[Debug]\n C --> A");
        let heads = art
            .chars()
            .filter(|ch| matches!(ch, '\u{25b6}' | '\u{25c0}' | '\u{25b2}' | '\u{25bc}'))
            .count();
        assert_eq!(heads, 3, "every edge keeps its head: {art}");
    }

    #[test]
    fn two_branch_labels_stay_whole() {
        let art = render(
            "flowchart LR\n A[Run] --> B{Pass?}\n B -->|No| C[Debug]\n C --> A\n B -->|Yes| D[Ship]",
        );
        assert_eq!(art.matches("Yes").count(), 1, "{art}");
        assert_eq!(art.matches("No").count(), 1, "{art}");
    }

    #[test_case("A[/One<br/>Two/]", LEAN_RIGHT_STROKE, LEAN_RIGHT_STROKE ; "lean_right")]
    #[test_case(r"A[\One<br/>Two\]", LEAN_LEFT_STROKE, LEAN_LEFT_STROKE   ; "lean_left")]
    #[test_case(r"A[/One<br/>Two\]", LEAN_RIGHT_STROKE, LEAN_LEFT_STROKE   ; "trapezoid_down")]
    #[test_case(r"A[\One<br/>Two/]", LEAN_LEFT_STROKE, LEAN_RIGHT_STROKE   ; "trapezoid_up")]
    fn a_leaning_side_steps_one_column_per_row(decl: &str, leading: char, trailing: char) {
        let art = render(&format!("flowchart LR\n  {decl} --> B[Next]"));
        let rows: Vec<&str> = art
            .lines()
            .filter(|line| line.contains("One") || line.contains("Two"))
            .collect();
        assert_eq!(rows.len(), 2, "expected two label rows: {art}");

        let column = |line: &str, stroke: char, from_left: bool| {
            match from_left {
                true => line.find(stroke),
                false => line.rfind(stroke),
            }
            .unwrap_or_else(|| panic!("no {stroke} in {line:?}: {art}"))
        };

        for (stroke, from_left) in [(leading, true), (trailing, false)] {
            let step = column(rows[1], stroke, from_left) as isize
                - column(rows[0], stroke, from_left) as isize;
            let expected = if stroke == LEAN_RIGHT_STROKE { -1 } else { 1 };
            assert_eq!(step, expected, "{stroke} should step one column: {art}");
        }
    }

    #[test]
    fn a_leaning_node_shows_no_delimiter_in_its_text() {
        let art = render("flowchart LR\n  A[/\"PEFT adapter directory\"/] --> B[Next]");
        assert!(art.contains("PEFT adapter directory"), "{art}");
        assert!(!art.contains('"'), "quotes are syntax, not text: {art}");
    }

    #[test]
    fn thick_edges_use_heavy_glyphs() {
        let art = render("flowchart TD\n  A ==> B");
        assert!(art.contains('┃'), "{art}");
    }

    #[test]
    fn dotted_edges_use_dashed_glyphs() {
        let art = render("flowchart TD\n  A -.-> B");
        assert!(art.contains(DOTTED_VERTICAL), "{art}");
    }

    #[test]
    fn edge_labels_are_written() {
        let art = render("flowchart TD\n  A -->|yes| B");
        assert!(art.contains("yes"), "{art}");
    }

    #[test]
    fn every_row_is_padded_to_the_canvas_width() {
        let graph = parse::parse("flowchart TD\n  A --> B\n  A --> C").unwrap();
        let placed = layout::layout(&graph);
        let canvas = paint(&graph, &placed);
        assert!(canvas.rows().all(|row| row.len() == canvas.width));
    }

    #[test]
    fn open_links_have_no_arrowhead() {
        let art = render("flowchart TD\n  A --- B");
        assert!(!art.contains('▼'), "{art}");
    }
}
