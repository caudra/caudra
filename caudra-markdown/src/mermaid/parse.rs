//! Mermaid flowchart parser.
//!
//! Strict by design. Anything outside the supported grammar is an error so
//! the caller can fall back to rendering the fence as a code block. Drawing a
//! diagram we only half understood is worse than drawing none at all.

use std::collections::HashMap;

use thiserror::Error;

const COMMENT: &str = "%%";
const SUBGRAPH: &str = "subgraph";
const END: &str = "end";
const DIRECTION: &str = "direction";
const FLOWCHART_KEYWORDS: [&str; 2] = ["flowchart", "graph"];
const IGNORED_KEYWORDS: [&str; 6] = [
    "style",
    "classDef",
    "class",
    "click",
    "linkStyle",
    "accTitle",
];
const LINE_BREAKS: [&str; 3] = ["<br/>", "<br />", "<br>"];
const ARROW_HEADS: [char; 3] = ['>', 'o', 'x'];
const STATEMENT_SEPARATOR: char = ';';
const FAN_OUT: char = '&';
const LABEL_PIPE: char = '|';
const QUOTE: char = '"';

/// Longer openers first so `([` is not mistaken for `(`. The leaning pairs
/// share an opener and differ only in how they close, so they are tried in
/// turn and a missing closer falls through to the next candidate.
const SHAPES: [(&str, &str, Shape); 13] = [
    ("([", "])", Shape::Stadium),
    ("[[", "]]", Shape::Subroutine),
    ("[(", ")]", Shape::Cylinder),
    ("((", "))", Shape::Circle),
    ("{{", "}}", Shape::Hexagon),
    ("[/", "/]", Shape::LeanRight),
    ("[/", "\\]", Shape::TrapezoidDown),
    ("[\\", "\\]", Shape::LeanLeft),
    ("[\\", "/]", Shape::TrapezoidUp),
    ("[", "]", Shape::Rect),
    ("(", ")", Shape::Round),
    ("{", "}", Shape::Rhombus),
    (">", "]", Shape::Asymmetric),
];

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ParseError {
    #[error("not a flowchart")]
    NotFlowchart,
    #[error("unsupported syntax: {0}")]
    Unsupported(String),
    #[error("nested subgraphs are not supported")]
    NestedSubgraph,
    #[error("`end` without a matching `subgraph`")]
    UnmatchedEnd,
    #[error("self-loops are not supported")]
    SelfLoop,
    #[error("unterminated subgraph")]
    UnterminatedSubgraph,
    #[error("diagram has no nodes")]
    Empty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Down,
    Up,
    Left,
    Right,
}

impl Direction {
    /// Ranks advance along the vertical axis for `TD`/`BT`, horizontal for
    /// `LR`/`RL`. Layout works in rank/order space and only consults this
    /// when mapping to screen coordinates.
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Down | Self::Up)
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "TD" | "TB" => Some(Self::Down),
            "BT" => Some(Self::Up),
            "LR" => Some(Self::Right),
            "RL" => Some(Self::Left),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shape {
    Rect,
    Round,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    Rhombus,
    Hexagon,
    Asymmetric,
    /// `[/text/]`, mermaid's data input/output.
    LeanRight,
    /// `[\text\]`, the same shape mirrored.
    LeanLeft,
    /// `[/text\]`, wider along its bottom edge.
    TrapezoidDown,
    /// `[\text/]`, wider along its top edge.
    TrapezoidUp,
}

impl Shape {
    /// Columns the left and right borders travel per row going down, or
    /// `None` when the sides are upright.
    pub fn lean(self) -> Option<(isize, isize)> {
        match self {
            Self::LeanRight => Some((-1, -1)),
            Self::LeanLeft => Some((1, 1)),
            Self::TrapezoidDown => Some((-1, 1)),
            Self::TrapezoidUp => Some((1, -1)),
            _ => None,
        }
    }

    /// Columns a slant adds to a box `height` rows tall. Sides that travel the
    /// same way shift the box; sides that diverge widen it twice over.
    pub fn shear(self, height: usize) -> usize {
        self.lean().map_or(0, |(left, right)| {
            height.saturating_sub(1) * if left == right { 1 } else { 2 }
        })
    }

    /// Columns the two borders occupy on `row`, relative to the box's left
    /// edge. The offsets are anchored so the widest row spans the whole box.
    pub fn sides(self, width: usize, height: usize, row: usize) -> (usize, usize) {
        let far = width.saturating_sub(1) as isize;
        let Some((left_step, right_step)) = self.lean() else {
            return (0, far.max(0) as usize);
        };
        let last = height.saturating_sub(1) as isize;
        let row = (row as isize).min(last);
        let left = if left_step < 0 { last } else { 0 } + left_step * row;
        let right = if right_step > 0 { far - last } else { far } + right_step * row;
        (left.clamp(0, far) as usize, right.clamp(0, far) as usize)
    }

    /// Columns covered by both the top and the bottom rule. An edge meeting a
    /// horizontal face has to land here, or it arrives beside the slant
    /// instead of on it.
    pub fn rule_window(self, width: usize, height: usize) -> (usize, usize) {
        let top = self.sides(width, height, 0);
        let bottom = self.sides(width, height, height.saturating_sub(1));
        (
            top.0.max(bottom.0),
            top.1.min(bottom.1).max(top.0.max(bottom.0)),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeStyle {
    Solid,
    Dotted,
    Thick,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    pub id: String,
    pub label: Vec<String>,
    pub shape: Shape,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Option<String>,
    pub style: EdgeStyle,
    pub arrow: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subgraph {
    pub title: String,
    pub nodes: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Graph {
    pub direction: Direction,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub subgraphs: Vec<Subgraph>,
}

/// One edge operator with its label, as read off the statement.
struct EdgeOp {
    style: EdgeStyle,
    arrow: bool,
    label: Option<String>,
    len: usize,
}

/// A node reference with the shape and label it was declared with, if any.
struct NodeRef {
    id: String,
    label: Option<Vec<String>>,
    shape: Option<Shape>,
    len: usize,
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    index: HashMap<String, usize>,
    edges: Vec<Edge>,
    subgraphs: Vec<Subgraph>,
}

impl Builder {
    /// Later declarations upgrade a node that was first seen as a bare
    /// reference, which is how `A --> B` followed by `B[Label]` behaves.
    fn intern(&mut self, node: NodeRef) -> usize {
        let idx = match self.index.get(&node.id) {
            Some(&known) => known,
            None => {
                let fresh = self.nodes.len();
                self.nodes.push(Node {
                    label: vec![node.id.clone()],
                    id: node.id.clone(),
                    shape: Shape::Rect,
                });
                self.index.insert(node.id.clone(), fresh);
                fresh
            }
        };
        if let Some(label) = node.label {
            self.nodes[idx].label = label;
        }
        if let Some(shape) = node.shape {
            self.nodes[idx].shape = shape;
        }
        idx
    }
}

pub fn parse(source: &str) -> Result<Graph, ParseError> {
    let mut lines = source
        .lines()
        .map(strip_comment)
        .map(str::trim)
        .filter(|line| !line.is_empty());

    let header = lines.next().ok_or(ParseError::NotFlowchart)?;
    let direction = parse_header(header)?;

    let mut builder = Builder::default();
    let mut open: Option<Subgraph> = None;

    for line in lines {
        for statement in line.split(STATEMENT_SEPARATOR).map(str::trim) {
            if statement.is_empty() {
                continue;
            }
            match classify(statement) {
                Statement::Ignored => {}
                Statement::SubgraphStart(title) => {
                    if open.is_some() {
                        return Err(ParseError::NestedSubgraph);
                    }
                    open = Some(Subgraph {
                        title,
                        nodes: Vec::new(),
                    });
                }
                Statement::SubgraphEnd => {
                    let done = open.take().ok_or(ParseError::UnmatchedEnd)?;
                    builder.subgraphs.push(done);
                }
                Statement::Chain => {
                    let touched = parse_chain(statement, &mut builder)?;
                    if let Some(group) = open.as_mut() {
                        for idx in touched {
                            if !group.nodes.contains(&idx) {
                                group.nodes.push(idx);
                            }
                        }
                    }
                }
            }
        }
    }

    if open.is_some() {
        return Err(ParseError::UnterminatedSubgraph);
    }
    if builder.nodes.is_empty() {
        return Err(ParseError::Empty);
    }

    Ok(Graph {
        direction,
        nodes: builder.nodes,
        edges: builder.edges,
        subgraphs: builder.subgraphs,
    })
}

enum Statement {
    Ignored,
    SubgraphStart(String),
    SubgraphEnd,
    Chain,
}

fn classify(statement: &str) -> Statement {
    let head = statement.split_whitespace().next().unwrap_or_default();
    if head == END {
        return Statement::SubgraphEnd;
    }
    if head == DIRECTION || IGNORED_KEYWORDS.contains(&head) {
        return Statement::Ignored;
    }
    if head == SUBGRAPH {
        return Statement::SubgraphStart(subgraph_title(statement[SUBGRAPH.len()..].trim()));
    }
    Statement::Chain
}

/// `subgraph id[Title]` names the box `Title`; `subgraph Plain Words` uses
/// the whole remainder, which is why this cannot just read a node.
fn subgraph_title(rest: &str) -> String {
    if rest.is_empty() {
        return String::new();
    }
    match read_node(rest) {
        Some(node) if node.len == rest.len() => match node.label {
            Some(label) => label.join(" "),
            None => node.id,
        },
        _ => unquote(rest).to_owned(),
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find(COMMENT) {
        Some(at) => &line[..at],
        None => line,
    }
}

fn parse_header(header: &str) -> Result<Direction, ParseError> {
    let mut tokens = header.split_whitespace();
    let keyword = tokens.next().ok_or(ParseError::NotFlowchart)?;
    if !FLOWCHART_KEYWORDS.contains(&keyword) {
        return Err(ParseError::NotFlowchart);
    }
    match tokens.next() {
        None => Ok(Direction::Down),
        Some(token) => Direction::parse(token.trim_end_matches(STATEMENT_SEPARATOR))
            .ok_or_else(|| ParseError::Unsupported(token.to_owned())),
    }
}

/// Reads `A --> B & C --> D`, returning every node index the statement
/// touched so an enclosing subgraph can claim them.
fn parse_chain(statement: &str, builder: &mut Builder) -> Result<Vec<usize>, ParseError> {
    let mut touched = Vec::new();
    let mut rest = statement;
    let mut previous = read_group(&mut rest, builder, statement)?;
    touched.extend_from_slice(&previous);

    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let op = read_edge(rest).ok_or_else(|| ParseError::Unsupported(statement.to_owned()))?;
        rest = &rest[op.len..];
        let next = read_group(&mut rest, builder, statement)?;
        touched.extend_from_slice(&next);
        for &from in &previous {
            for &to in &next {
                if from == to {
                    return Err(ParseError::SelfLoop);
                }
                builder.edges.push(Edge {
                    from,
                    to,
                    label: op.label.clone(),
                    style: op.style,
                    arrow: op.arrow,
                });
            }
        }
        previous = next;
    }
    Ok(touched)
}

/// A group is one node, or several joined by `&`.
fn read_group(
    rest: &mut &str,
    builder: &mut Builder,
    statement: &str,
) -> Result<Vec<usize>, ParseError> {
    let mut group = Vec::new();
    loop {
        *rest = rest.trim_start();
        let node = read_node(rest).ok_or_else(|| ParseError::Unsupported(statement.to_owned()))?;
        *rest = &rest[node.len..];
        group.push(builder.intern(node));

        let after = rest.trim_start();
        match after.starts_with(FAN_OUT) {
            true => *rest = &after[FAN_OUT.len_utf8()..],
            false => return Ok(group),
        }
    }
}

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn read_node(input: &str) -> Option<NodeRef> {
    let id_len = input.find(|c| !is_id_char(c)).unwrap_or(input.len());
    if id_len == 0 {
        return None;
    }
    let id = input[..id_len].to_owned();
    let rest = &input[id_len..];

    for (open, close, shape) in SHAPES {
        if !rest.starts_with(open) {
            continue;
        }
        let body = &rest[open.len()..];
        // Two shapes can share an opener, so an absent closer means the wrong
        // candidate rather than a malformed node.
        let Some(end) = find_close(body, close) else {
            continue;
        };
        return Some(NodeRef {
            id,
            label: Some(split_label(&body[..end])),
            shape: Some(shape),
            len: id_len + open.len() + end + close.len(),
        });
    }

    Some(NodeRef {
        id,
        label: None,
        shape: None,
        len: id_len,
    })
}

/// Quoted labels may contain the closing delimiter, so the scan has to step
/// over quoted runs rather than search the whole body.
fn find_close(body: &str, close: &str) -> Option<usize> {
    let mut at = 0;
    while at < body.len() {
        let rest = &body[at..];
        if rest.starts_with(QUOTE) {
            let end = rest[QUOTE.len_utf8()..].find(QUOTE)? + QUOTE.len_utf8();
            at += end + QUOTE.len_utf8();
            continue;
        }
        if rest.starts_with(close) {
            return Some(at);
        }
        at += rest.chars().next()?.len_utf8();
    }
    None
}

fn unquote(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix(QUOTE)
        .and_then(|rest| rest.strip_suffix(QUOTE))
        .unwrap_or(trimmed)
}

fn split_label(raw: &str) -> Vec<String> {
    let mut text = unquote(raw).to_owned();
    for tag in LINE_BREAKS {
        text = text.replace(tag, "\n");
    }
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    match lines.is_empty() {
        true => vec![String::new()],
        false => lines,
    }
}

fn is_arrow_head(c: char) -> bool {
    ARROW_HEADS.contains(&c)
}

/// Recognises `-->`, `---`, `-.->`, `==>` and their labelled forms, both
/// `-->|text|` and `-- text -->`.
fn read_edge(input: &str) -> Option<EdgeOp> {
    let bytes = input.as_bytes();
    let stem = *bytes.first()?;
    if stem != b'-' && stem != b'=' {
        return None;
    }
    let body = |from: usize| {
        let mut at = from;
        while at < bytes.len() && (bytes[at] == stem || bytes[at] == b'.') {
            at += 1;
        }
        at
    };

    let first = body(0);
    if first == 1 {
        return None;
    }
    let style = match (stem, input[..first].contains('.')) {
        (b'=', _) => EdgeStyle::Thick,
        (_, true) => EdgeStyle::Dotted,
        _ => EdgeStyle::Solid,
    };

    let mut at = first;
    let mut arrow = false;
    if at < bytes.len() && is_arrow_head(bytes[at] as char) {
        arrow = true;
        at += 1;
    }

    if at < bytes.len() && bytes[at] == LABEL_PIPE as u8 {
        let end = input[at + 1..].find(LABEL_PIPE)? + at + 1;
        return Some(EdgeOp {
            style,
            arrow,
            label: Some(unquote(&input[at + 1..end]).to_owned()),
            len: end + 1,
        });
    }
    if arrow {
        return Some(EdgeOp {
            style,
            arrow,
            label: None,
            len: at,
        });
    }

    match find_edge_close(input, at, stem) {
        Some(close) => {
            let label = input[at..close].trim();
            let after = body(close);
            let arrow = after < bytes.len() && is_arrow_head(bytes[after] as char);
            Some(EdgeOp {
                style,
                arrow,
                label: (!label.is_empty()).then(|| unquote(label).to_owned()),
                len: after + usize::from(arrow),
            })
        }
        None => Some(EdgeOp {
            style,
            arrow,
            label: None,
            len: at,
        }),
    }
}

/// The closing half of `-- text -->`. A run of edge characters only closes
/// the edge when it continues, which is what keeps a hyphen inside the label
/// from ending it early.
fn find_edge_close(input: &str, from: usize, stem: u8) -> Option<usize> {
    let bytes = input.as_bytes();
    let is_edge = |b: u8| b == stem || b == b'.';
    for at in from..bytes.len() {
        if !is_edge(bytes[at]) {
            continue;
        }
        let next = *bytes.get(at + 1)?;
        if is_edge(next) || is_arrow_head(next as char) {
            return Some(at);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn graph(source: &str) -> Graph {
        parse(source).expect("fixture should parse")
    }

    fn edge_labels(g: &Graph) -> Vec<Option<&str>> {
        g.edges.iter().map(|e| e.label.as_deref()).collect()
    }

    #[test_case("flowchart TD", Direction::Down     ; "flowchart_td")]
    #[test_case("flowchart TB", Direction::Down     ; "flowchart_tb")]
    #[test_case("flowchart BT", Direction::Up       ; "flowchart_bt")]
    #[test_case("flowchart LR", Direction::Right    ; "flowchart_lr")]
    #[test_case("flowchart RL", Direction::Left     ; "flowchart_rl")]
    #[test_case("graph TD", Direction::Down         ; "graph_keyword")]
    #[test_case("flowchart", Direction::Down        ; "bare_defaults_down")]
    fn header_directions(header: &str, expected: Direction) {
        let g = graph(&format!("{header}\n  A --> B"));
        assert_eq!(g.direction, expected);
    }

    #[test_case("sequenceDiagram\n  A ->> B: hi"   ; "sequence_diagram")]
    #[test_case("classDiagram\n  A <|-- B"          ; "class_diagram")]
    #[test_case("pie title X"                       ; "pie_chart")]
    #[test_case(""                                  ; "empty_source")]
    fn rejects_non_flowcharts(source: &str) {
        assert!(parse(source).is_err());
    }

    #[test]
    fn rejects_unknown_direction() {
        assert_eq!(
            parse("flowchart SIDEWAYS\n A --> B"),
            Err(ParseError::Unsupported("SIDEWAYS".to_owned()))
        );
    }

    #[test_case("A[Rect]", Shape::Rect              ; "rect")]
    #[test_case("A(Round)", Shape::Round            ; "round")]
    #[test_case("A([Stadium])", Shape::Stadium      ; "stadium")]
    #[test_case("A[[Sub]]", Shape::Subroutine       ; "subroutine")]
    #[test_case("A[(Cyl)]", Shape::Cylinder         ; "cylinder")]
    #[test_case("A((Circle))", Shape::Circle        ; "circle")]
    #[test_case("A{Rhombus}", Shape::Rhombus        ; "rhombus")]
    #[test_case("A{{Hex}}", Shape::Hexagon          ; "hexagon")]
    #[test_case("A>Flag]", Shape::Asymmetric        ; "asymmetric")]
    #[test_case("A[/Lean/]", Shape::LeanRight       ; "lean_right")]
    #[test_case(r"A[\Lean\]", Shape::LeanLeft       ; "lean_left")]
    #[test_case(r"A[/Trap\]", Shape::TrapezoidDown  ; "trapezoid_down")]
    #[test_case(r"A[\Trap/]", Shape::TrapezoidUp    ; "trapezoid_up")]
    fn node_shapes(decl: &str, expected: Shape) {
        let g = graph(&format!("flowchart TD\n  {decl} --> B"));
        assert_eq!(g.nodes[0].shape, expected);
    }

    #[test_case(Shape::Rect, 0             ; "upright_pays_nothing")]
    #[test_case(Shape::LeanRight, 4        ; "parallelogram_shifts_once")]
    #[test_case(Shape::LeanLeft, 4         ; "mirrored_shifts_once")]
    #[test_case(Shape::TrapezoidDown, 8    ; "trapezoid_widens_twice")]
    #[test_case(Shape::TrapezoidUp, 8      ; "inverted_trapezoid_widens_twice")]
    fn a_slant_costs_a_column_per_row(shape: Shape, expected: usize) {
        const HEIGHT: usize = 5;
        assert_eq!(shape.shear(HEIGHT), expected);
    }

    #[test_case(Shape::LeanRight     ; "lean_right")]
    #[test_case(Shape::LeanLeft      ; "lean_left")]
    #[test_case(Shape::TrapezoidDown ; "trapezoid_down")]
    #[test_case(Shape::TrapezoidUp   ; "trapezoid_up")]
    fn a_slant_fills_its_box_without_leaving_it(shape: Shape) {
        const TEXT: usize = 10;
        const HEIGHT: usize = 5;
        const BORDERS: usize = 2;
        let width = TEXT + BORDERS + shape.shear(HEIGHT);
        let rows: Vec<(usize, usize)> = (0..HEIGHT)
            .map(|row| shape.sides(width, HEIGHT, row))
            .collect();

        assert_eq!(rows.iter().map(|&(left, _)| left).min(), Some(0));
        assert_eq!(
            rows.iter().map(|&(_, right)| right).max(),
            Some(width - 1),
            "the widest row spans the box: {rows:?}"
        );
        assert_eq!(
            rows.iter().map(|&(l, r)| r - l - 1).min(),
            Some(TEXT),
            "the narrowest row still holds the text: {rows:?}"
        );
        for pair in rows.windows(2) {
            let ((left, right), (next_left, next_right)) = (pair[0], pair[1]);
            assert_eq!(next_left.abs_diff(left), 1, "left steps once: {rows:?}");
            assert_eq!(next_right.abs_diff(right), 1, "right steps once: {rows:?}");
        }
    }

    #[test_case(Shape::LeanRight     ; "lean_right")]
    #[test_case(Shape::TrapezoidDown ; "trapezoid_down")]
    fn a_horizontal_face_only_offers_what_both_rules_cover(shape: Shape) {
        const HEIGHT: usize = 4;
        let width = 12 + shape.shear(HEIGHT);
        let (low, high) = shape.rule_window(width, HEIGHT);
        for row in [0, HEIGHT - 1] {
            let (left, right) = shape.sides(width, HEIGHT, row);
            assert!(
                left <= low && high <= right,
                "row {row} spans {left}..={right}, outside the window {low}..={high}"
            );
        }
    }

    /// The leaning forms used to fall through to `[`, which kept the slashes
    /// and the quotes as part of the text.
    #[test_case("A[/Data/]", "Data"                          ; "bare")]
    #[test_case("A[/\"Quoted, text\"/]", "Quoted, text"      ; "quoted")]
    #[test_case(r"A[\Data\]", "Data"                         ; "mirrored")]
    #[test_case("A[/usr/bin]", "/usr/bin"                    ; "a_path_is_not_a_shape")]
    fn a_leaning_node_keeps_its_delimiters_out_of_the_label(decl: &str, expected: &str) {
        let g = graph(&format!("flowchart TD\n  {decl} --> B"));
        assert_eq!(g.nodes[0].label, vec![expected.to_owned()]);
    }

    #[test_case("A --> B", EdgeStyle::Solid, true   ; "solid_arrow")]
    #[test_case("A --- B", EdgeStyle::Solid, false  ; "solid_open")]
    #[test_case("A -.-> B", EdgeStyle::Dotted, true ; "dotted_arrow")]
    #[test_case("A -.- B", EdgeStyle::Dotted, false ; "dotted_open")]
    #[test_case("A ==> B", EdgeStyle::Thick, true   ; "thick_arrow")]
    #[test_case("A === B", EdgeStyle::Thick, false  ; "thick_open")]
    #[test_case("A ----> B", EdgeStyle::Solid, true ; "long_arrow")]
    fn edge_styles(statement: &str, style: EdgeStyle, arrow: bool) {
        let g = graph(&format!("flowchart TD\n  {statement}"));
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].style, style);
        assert_eq!(g.edges[0].arrow, arrow);
    }

    #[test_case("A -->|Yes| B", Some("Yes")             ; "pipe_label")]
    #[test_case("A -- Maybe --> B", Some("Maybe")       ; "inline_label")]
    #[test_case("A -. Later .-> B", Some("Later")       ; "dotted_inline_label")]
    #[test_case("A == Fast ==> B", Some("Fast")         ; "thick_inline_label")]
    #[test_case("A --> B", None                         ; "no_label")]
    #[test_case("A -- a-b --> B", Some("a-b")           ; "hyphen_inside_label")]
    fn edge_labels_are_read(statement: &str, expected: Option<&str>) {
        let g = graph(&format!("flowchart TD\n  {statement}"));
        assert_eq!(edge_labels(&g), vec![expected]);
    }

    #[test]
    fn chains_link_each_hop() {
        let g = graph("flowchart LR\n  A --> B --> C");
        assert_eq!(g.edges.len(), 2);
        assert_eq!((g.edges[0].from, g.edges[0].to), (0, 1));
        assert_eq!((g.edges[1].from, g.edges[1].to), (1, 2));
    }

    #[test]
    fn fan_out_expands_to_the_cross_product() {
        let g = graph("flowchart TD\n  A & B --> C & D");
        assert_eq!(g.edges.len(), 4);
        assert_eq!(g.nodes.len(), 4);
    }

    #[test]
    fn later_declaration_upgrades_a_bare_reference() {
        let g = graph("flowchart TD\n  A --> B\n  B{Decide}");
        assert_eq!(g.nodes[1].label, vec!["Decide".to_owned()]);
        assert_eq!(g.nodes[1].shape, Shape::Rhombus);
    }

    #[test]
    fn bare_reference_labels_itself_with_its_id() {
        let g = graph("flowchart TD\n  Alpha --> Beta");
        assert_eq!(g.nodes[0].label, vec!["Alpha".to_owned()]);
    }

    #[test]
    fn quoted_labels_keep_delimiter_characters() {
        let g = graph("flowchart TD\n  A[\"a] b\"] --> B");
        assert_eq!(g.nodes[0].label, vec!["a] b".to_owned()]);
    }

    #[test]
    fn line_break_tags_split_the_label() {
        let g = graph("flowchart TD\n  A[one<br/>two] --> B");
        assert_eq!(g.nodes[0].label, vec!["one".to_owned(), "two".to_owned()]);
    }

    #[test]
    fn comments_and_directives_are_skipped() {
        let g = graph(
            "flowchart TD\n  %% a comment\n  A --> B\n  style A fill:#f9f\n  classDef x fill:#000",
        );
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn trailing_comment_does_not_eat_the_statement() {
        let g = graph("flowchart TD\n  A --> B %% link them");
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn semicolons_separate_statements() {
        let g = graph("flowchart TD\n  A --> B; B --> C");
        assert_eq!(g.edges.len(), 2);
    }

    #[test]
    fn subgraph_collects_the_nodes_it_encloses() {
        let g = graph("flowchart TD\n  subgraph Group\n    A --> B\n  end\n  B --> C");
        assert_eq!(g.subgraphs.len(), 1);
        assert_eq!(g.subgraphs[0].title, "Group");
        assert_eq!(g.subgraphs[0].nodes, vec![0, 1]);
    }

    #[test]
    fn subgraph_title_may_be_bracketed() {
        let g = graph("flowchart TD\n  subgraph one[Nice Title]\n    A --> B\n  end");
        assert_eq!(g.subgraphs[0].title, "Nice Title");
    }

    #[test]
    fn subgraph_title_may_be_bare_words() {
        let g = graph("flowchart TD\n  subgraph Two Words\n    A --> B\n  end");
        assert_eq!(g.subgraphs[0].title, "Two Words");
    }

    #[test]
    fn subgraph_direction_is_accepted_and_ignored() {
        let g = graph("flowchart TD\n  subgraph G\n    direction LR\n    A --> B\n  end");
        assert_eq!(g.subgraphs[0].nodes, vec![0, 1]);
    }

    #[test_case("flowchart TD\n subgraph A\n  subgraph B\n  end\n end", ParseError::NestedSubgraph ; "nested")]
    #[test_case("flowchart TD\n A --> B\n end", ParseError::UnmatchedEnd                          ; "unmatched_end")]
    #[test_case("flowchart TD\n subgraph G\n  A --> B", ParseError::UnterminatedSubgraph          ; "unterminated")]
    fn subgraph_errors(source: &str, expected: ParseError) {
        assert_eq!(parse(source), Err(expected));
    }

    #[test_case("flowchart TD\n  A --> "        ; "dangling_edge")]
    #[test_case("flowchart TD\n  A[unclosed"    ; "unclosed_shape")]
    #[test_case("flowchart TD\n  --> B"         ; "leading_edge")]
    #[test_case("flowchart TD\n  A ~~~ B"       ; "unknown_operator")]
    fn malformed_statements_are_rejected(source: &str) {
        assert!(parse(source).is_err());
    }

    #[test]
    fn header_only_has_no_nodes() {
        assert_eq!(parse("flowchart TD"), Err(ParseError::Empty));
    }

    #[test]
    fn standalone_node_needs_no_edge() {
        let g = graph("flowchart TD\n  Solo[All alone]");
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty());
    }
}
