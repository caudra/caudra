//! Mermaid state diagram parser.
//!
//! A state diagram is translated into the flowchart `Graph`, so layout and
//! paint serve both. States become rounded boxes and `[*]` becomes a start or
//! an end marker of the region it is written in. Every region of a composite
//! state becomes a subgraph holding the states first seen directly inside it.
//! Subgraphs do not nest, so a composite inside another is framed on its own
//! beside its parent. A composite that a transition starts or ends at is
//! drawn as a box inside its own frame, or on its own when it holds nothing
//! but other composites.
//!
//! Styling (`classDef`, `class`, `:::name`) and notes are read and dropped.
//! Forks, joins and anything else outside this grammar are errors, so the
//! caller falls back to a code block.

use std::collections::HashMap;

use super::parse::{
    Direction, Edge, EdgeStyle, Graph, Node, ParseError, QUOTE, Shape, Subgraph, is_id_char,
    split_label,
};

const STATE: &str = "state";
const AS: &str = "as";
const TRANSITION: &str = "-->";
const PSEUDO_STATE: &str = "[*]";
const CLASS_SHORTHAND: &str = ":::";
const DESCRIPTION: char = ':';
const OPEN_COMPOSITE: &str = "{";
const CLOSE_COMPOSITE: &str = "}";
const REGION_SEPARATOR: &str = "--";
const DIRECTION: &str = "direction";
const NOTE: &str = "note";
const END_NOTE: [&str; 2] = ["end", "note"];
const CHOICE: &str = "<<choice>>";
const STATEMENT_END: char = ';';
const IGNORED_KEYWORDS: [&str; 5] = ["classDef", "class", "style", "accTitle", "accDescr"];
const START_MARK: &str = "●";
const END_MARK: &str = "◉";
const WORD_SEPARATOR: &str = " ";

/// The region of a composite state a statement is written in, or `None` at
/// the top level.
type Region = Option<usize>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Endpoint {
    State(usize),
    Start(Region),
    End(Region),
}

enum Reference<'a> {
    Pseudo,
    Named(&'a str),
}

struct State {
    id: String,
    label: Option<Vec<String>>,
    shape: Shape,
    region: Region,
    /// The regions this state encloses. Empty for a simple state.
    regions: Vec<usize>,
}

impl State {
    fn is_composite(&self) -> bool {
        !self.regions.is_empty()
    }

    fn title(&self) -> String {
        match &self.label {
            Some(lines) => lines.join(WORD_SEPARATOR),
            None => self.id.clone(),
        }
    }
}

struct Transition {
    from: Endpoint,
    to: Endpoint,
    label: Option<String>,
}

struct Chart {
    direction: Direction,
    states: Vec<State>,
    index: HashMap<String, usize>,
    /// The composite state owning each region.
    owners: Vec<usize>,
    /// Regions currently open, innermost last.
    open: Vec<usize>,
    /// States and markers in the order they first appear, which is the order
    /// they become nodes in.
    appearance: Vec<Endpoint>,
    transitions: Vec<Transition>,
}

pub(super) fn parse<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Graph, ParseError> {
    let mut chart = Chart {
        direction: Direction::Down,
        states: Vec::new(),
        index: HashMap::new(),
        owners: Vec::new(),
        open: Vec::new(),
        appearance: Vec::new(),
        transitions: Vec::new(),
    };
    let mut in_note = false;
    for line in lines {
        if in_note {
            in_note = !line.split_whitespace().eq(END_NOTE);
            continue;
        }
        let statement = line.trim_end_matches(STATEMENT_END).trim_end();
        if opens_note(statement) {
            in_note = true;
            continue;
        }
        chart.statement(statement)?;
    }
    if in_note {
        return Err(ParseError::UnterminatedNote);
    }
    if !chart.open.is_empty() {
        return Err(ParseError::UnterminatedComposite);
    }
    chart.build()
}

/// A note with neither `: text` nor a quoted body runs until `end note`.
fn opens_note(statement: &str) -> bool {
    statement
        .strip_prefix(NOTE)
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        && !statement.contains(DESCRIPTION)
        && !statement.contains(QUOTE)
}

/// Reads `[*]` or a state id, then drops a `:::name` style suffix.
fn read_reference(input: &str) -> Option<(Reference<'_>, &str)> {
    let (reference, rest) = match input.strip_prefix(PSEUDO_STATE) {
        Some(rest) => (Reference::Pseudo, rest),
        None => {
            let len = input.find(|c| !is_id_char(c)).unwrap_or(input.len());
            if len == 0 {
                return None;
            }
            (Reference::Named(&input[..len]), &input[len..])
        }
    };
    let rest = match rest.strip_prefix(CLASS_SHORTHAND) {
        Some(class) => class.find(|c| !is_id_char(c)).map_or("", |at| &class[at..]),
        None => rest,
    };
    Some((reference, rest))
}

/// Edge labels are drawn on one line, so mermaid's line breaks become spaces.
fn edge_label(text: &str) -> Option<String> {
    let label = split_label(text).join(WORD_SEPARATOR);
    (!label.is_empty()).then_some(label)
}

impl Chart {
    fn region(&self) -> Region {
        self.open.last().copied()
    }

    fn statement(&mut self, statement: &str) -> Result<(), ParseError> {
        let unsupported = || ParseError::Unsupported(statement.to_owned());
        let head = statement.split_whitespace().next().unwrap_or_default();
        match statement {
            CLOSE_COMPOSITE => self.open.pop().map(drop).ok_or(ParseError::UnmatchedBrace),
            REGION_SEPARATOR => {
                let current = self.open.last_mut().ok_or_else(unsupported)?;
                let owner = self.owners[*current];
                *current = self.owners.len();
                self.owners.push(owner);
                self.states[owner].regions.push(*current);
                Ok(())
            }
            _ if head == DIRECTION => {
                let token = statement[DIRECTION.len()..].trim();
                let direction = Direction::parse(token).ok_or_else(unsupported)?;
                if self.open.is_empty() {
                    self.direction = direction;
                }
                Ok(())
            }
            _ if head == NOTE => Ok(()),
            _ if head == STATE => self.declare(statement[STATE.len()..].trim_start(), statement),
            _ if IGNORED_KEYWORDS.contains(&head)
                && !statement.contains(TRANSITION)
                && !statement.ends_with(OPEN_COMPOSITE) =>
            {
                Ok(())
            }
            _ => self.transition_or_description(statement),
        }
    }

    /// `state "Label" as id`, `state id`, `state id <<choice>>`, each of
    /// which may open a composite with a trailing `{`.
    fn declare(&mut self, rest: &str, statement: &str) -> Result<(), ParseError> {
        let unsupported = || ParseError::Unsupported(statement.to_owned());
        let (label, rest) = match rest.strip_prefix(QUOTE) {
            Some(quoted) => {
                let end = quoted.find(QUOTE).ok_or_else(unsupported)?;
                let after = quoted[end + QUOTE.len_utf8()..].trim_start();
                let named = after
                    .strip_prefix(AS)
                    .filter(|named| named.starts_with(char::is_whitespace))
                    .ok_or_else(unsupported)?;
                (Some(split_label(&quoted[..end])), named.trim_start())
            }
            None => (None, rest),
        };
        let (Reference::Named(id), rest) = read_reference(rest).ok_or_else(unsupported)? else {
            return Err(unsupported());
        };
        let rest = rest.trim();
        let (choice, rest) = match rest.strip_prefix(CHOICE) {
            Some(rest) => (true, rest.trim_start()),
            None => (false, rest),
        };
        let opens = match rest {
            "" => false,
            OPEN_COMPOSITE => true,
            _ => return Err(unsupported()),
        };

        let state = self.intern(id);
        if choice {
            self.states[state].shape = Shape::Rhombus;
            self.states[state].label = Some(vec![String::new()]);
        }
        if label.is_some() {
            self.states[state].label = label;
        }
        if opens {
            let region = self.owners.len();
            self.owners.push(state);
            self.states[state].regions.push(region);
            self.open.push(region);
        }
        Ok(())
    }

    /// `A --> B`, `A --> B : label`, or `A : description`.
    fn transition_or_description(&mut self, statement: &str) -> Result<(), ParseError> {
        let unsupported = || ParseError::Unsupported(statement.to_owned());
        let (from, rest) = read_reference(statement).ok_or_else(unsupported)?;
        let rest = rest.trim_start();

        if let Some(after) = rest.strip_prefix(TRANSITION) {
            let (to, rest) = read_reference(after.trim_start()).ok_or_else(unsupported)?;
            let rest = rest.trim_start();
            let label = match rest.strip_prefix(DESCRIPTION) {
                Some(text) => edge_label(text),
                None if rest.is_empty() => None,
                None => return Err(unsupported()),
            };
            let from = self.endpoint(from, Endpoint::Start);
            let to = self.endpoint(to, Endpoint::End);
            self.transitions.push(Transition { from, to, label });
            return Ok(());
        }

        let Reference::Named(id) = from else {
            return Err(unsupported());
        };
        let state = self.intern(id);
        match rest.strip_prefix(DESCRIPTION) {
            Some(text) => {
                self.states[state]
                    .label
                    .get_or_insert_with(Vec::new)
                    .extend(split_label(text));
                Ok(())
            }
            None if rest.is_empty() => Ok(()),
            None => Err(unsupported()),
        }
    }

    /// `[*]` starts a region when it is the source and ends it when it is the
    /// target, so each role gets its own marker.
    fn endpoint(&mut self, reference: Reference<'_>, marker: fn(Region) -> Endpoint) -> Endpoint {
        match reference {
            Reference::Named(id) => Endpoint::State(self.intern(id)),
            Reference::Pseudo => {
                let endpoint = marker(self.region());
                if !self.appearance.contains(&endpoint) {
                    self.appearance.push(endpoint);
                }
                endpoint
            }
        }
    }

    fn intern(&mut self, id: &str) -> usize {
        if let Some(&known) = self.index.get(id) {
            return known;
        }
        let fresh = self.states.len();
        self.states.push(State {
            id: id.to_owned(),
            label: None,
            shape: Shape::Round,
            region: self.region(),
            regions: Vec::new(),
        });
        self.index.insert(id.to_owned(), fresh);
        self.appearance.push(Endpoint::State(fresh));
        fresh
    }

    fn build(self) -> Result<Graph, ParseError> {
        let mut anchored = vec![false; self.states.len()];
        for transition in &self.transitions {
            for endpoint in [transition.from, transition.to] {
                if let Endpoint::State(state) = endpoint {
                    anchored[state] |= self.states[state].is_composite();
                }
            }
        }

        let mut populated = vec![false; self.owners.len()];
        for &endpoint in &self.appearance {
            let region = match endpoint {
                Endpoint::State(state) if self.states[state].is_composite() => None,
                Endpoint::State(state) => self.states[state].region,
                Endpoint::Start(region) | Endpoint::End(region) => region,
            };
            if let Some(region) = region {
                populated[region] = true;
            }
        }

        let mut nodes = Vec::new();
        let mut members: Vec<Region> = Vec::new();
        let mut node_of: HashMap<Endpoint, usize> = HashMap::new();
        let mut anchor_regions: Vec<usize> = Vec::new();
        for &endpoint in &self.appearance {
            let (node, region) = match endpoint {
                Endpoint::State(state) if self.states[state].is_composite() => {
                    if !anchored[state] {
                        continue;
                    }
                    let composite = &self.states[state];
                    let region = composite
                        .regions
                        .iter()
                        .copied()
                        .find(|&region| populated[region]);
                    anchor_regions.extend(region);
                    (state_node(composite), region.or(composite.region))
                }
                Endpoint::State(state) => {
                    (state_node(&self.states[state]), self.states[state].region)
                }
                Endpoint::Start(region) => (marker_node(START_MARK), region),
                Endpoint::End(region) => (marker_node(END_MARK), region),
            };
            node_of.insert(endpoint, nodes.len());
            nodes.push(node);
            members.push(region);
        }
        if nodes.is_empty() {
            return Err(ParseError::Empty);
        }

        let subgraphs = (0..self.owners.len())
            .filter_map(|region| {
                let nodes: Vec<usize> = (0..members.len())
                    .filter(|&node| members[node] == Some(region))
                    .collect();
                if nodes.is_empty() {
                    return None;
                }
                // The anchor box already names the composite.
                let title = match anchor_regions.contains(&region) {
                    true => String::new(),
                    false => self.states[self.owners[region]].title(),
                };
                Some(Subgraph { title, nodes })
            })
            .collect();

        let edges = self
            .transitions
            .iter()
            .filter_map(|transition| {
                Some(Edge {
                    from: *node_of.get(&transition.from)?,
                    to: *node_of.get(&transition.to)?,
                    label: transition.label.clone(),
                    style: EdgeStyle::Solid,
                    arrow: true,
                })
            })
            .collect();

        Ok(Graph {
            direction: self.direction,
            nodes,
            edges,
            subgraphs,
        })
    }
}

fn state_node(state: &State) -> Node {
    Node {
        id: state.id.clone(),
        label: state
            .label
            .clone()
            .unwrap_or_else(|| vec![state.id.clone()]),
        shape: state.shape,
    }
}

fn marker_node(mark: &str) -> Node {
    Node {
        id: PSEUDO_STATE.to_owned(),
        label: vec![mark.to_owned()],
        shape: Shape::Circle,
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;
    use test_case::test_case;

    fn graph(body: &str) -> Graph {
        parse(&format!("stateDiagram-v2\n{body}")).expect("fixture should parse")
    }

    fn labels(g: &Graph) -> Vec<String> {
        g.nodes.iter().map(|node| node.label.join("\n")).collect()
    }

    fn subgraph<'a>(g: &'a Graph, title: &str) -> &'a Subgraph {
        g.subgraphs
            .iter()
            .find(|group| group.title == title)
            .unwrap_or_else(|| panic!("no subgraph {title:?} in {:?}", g.subgraphs))
    }

    fn member_labels(g: &Graph, group: &Subgraph) -> Vec<String> {
        group
            .nodes
            .iter()
            .map(|&node| g.nodes[node].label.join("\n"))
            .collect()
    }

    #[test_case("stateDiagram-v2" ; "v2")]
    #[test_case("stateDiagram"    ; "v1")]
    fn both_headers_start_a_state_diagram(header: &str) {
        let g = parse(&format!("{header}\n  A --> B")).expect("fixture should parse");
        assert_eq!(labels(&g), ["A", "B"]);
        assert_eq!(g.direction, Direction::Down);
    }

    #[test]
    fn top_level_direction_turns_the_chart() {
        assert_eq!(graph("direction LR\nA --> B").direction, Direction::Right);
    }

    #[test]
    fn a_composite_direction_leaves_the_chart_alone() {
        let g = graph("state C {\n direction LR\n A --> B\n}");
        assert_eq!(g.direction, Direction::Down);
    }

    #[test]
    fn start_and_end_are_separate_markers() {
        let g = graph("[*] --> A\nA --> [*]");
        assert_eq!(labels(&g), [START_MARK, "A", END_MARK]);
        assert_eq!(g.nodes[0].shape, Shape::Circle);
        assert_eq!(g.nodes[2].shape, Shape::Circle);
        assert_eq!(g.nodes[1].shape, Shape::Round);
    }

    #[test]
    fn a_declared_label_keeps_its_line_breaks() {
        let g = graph("state \"Waiting<br/>to sync\" as w\n[*] --> w");
        assert_eq!(g.nodes[0].label, ["Waiting", "to sync"]);
    }

    #[test]
    fn a_description_labels_the_state() {
        let g = graph("A : Ready to go\nA --> B");
        assert_eq!(g.nodes[0].label, ["Ready to go"]);
    }

    #[test_case("A --> B : you press Save", Some("you press Save")   ; "spaced")]
    #[test_case("A --> B: tight", Some("tight")                      ; "tight_colon")]
    #[test_case("A --> B : one<br/>two", Some("one two")             ; "line_break_becomes_space")]
    #[test_case("A --> B", None                                      ; "unlabelled")]
    #[test_case("A:::hot --> B:::cold : styled", Some("styled")      ; "class_shorthand")]
    fn transition_labels_are_read(statement: &str, expected: Option<&str>) {
        let g = graph(statement);
        assert_eq!(labels(&g), ["A", "B"]);
        assert_eq!(g.edges[0].label.as_deref(), expected);
    }

    #[test]
    fn a_self_transition_is_kept() {
        let g = graph("A --> A : retry");
        assert_eq!((g.edges[0].from, g.edges[0].to), (0, 0));
    }

    #[test]
    fn styling_and_notes_are_dropped() {
        let g = graph(
            "classDef bad fill:#f00;\nclass A,B bad\nstyle A fill:#0f0\n\
             note right of A : one line\nnote left of B\n  A --> C\nend note\n\
             note \"floating\" as N\nA --> B",
        );
        assert_eq!(labels(&g), ["A", "B"]);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn a_choice_is_a_decision() {
        let g = graph("state pick <<choice>>\n[*] --> pick");
        assert_eq!(g.nodes[0].shape, Shape::Rhombus);
    }

    #[test]
    fn a_composite_frames_the_states_first_seen_inside_it() {
        let g =
            graph("state \"Legend\" as legend {\n state \"Works\" as w\n}\nclass w works\nA --> B");
        assert_eq!(member_labels(&g, subgraph(&g, "Legend")), ["Works"]);
        assert_eq!(g.subgraphs.len(), 1);
    }

    #[test]
    fn each_region_gets_its_own_frame_and_markers() {
        let g = graph("state Both {\n [*] --> A\n --\n [*] --> B\n}");
        let frames: Vec<Vec<String>> = g
            .subgraphs
            .iter()
            .map(|group| member_labels(&g, group))
            .collect();
        assert_eq!(frames, [[START_MARK, "A"], [START_MARK, "B"]]);
        assert!(g.subgraphs.iter().all(|group| group.title == "Both"));
    }

    #[test]
    fn a_nested_composite_is_framed_beside_its_parent() {
        let g = graph("state Outer {\n X --> Y\n state Inner {\n  A --> B\n }\n}");
        assert_eq!(member_labels(&g, subgraph(&g, "Outer")), ["X", "Y"]);
        assert_eq!(member_labels(&g, subgraph(&g, "Inner")), ["A", "B"]);
    }

    #[test]
    fn a_composite_transition_lands_on_a_box_inside_its_frame() {
        let g =
            graph("state \"Uploading\" as Up {\n [*] --> Sending\n}\nIdle --> Up\nUp --> Failed");
        let frame = &g.subgraphs[0];
        assert_eq!(
            member_labels(&g, frame),
            ["Uploading", START_MARK, "Sending"]
        );
        assert!(frame.title.is_empty(), "the box names the frame");
        let anchor = frame.nodes[0];
        let ends: Vec<(usize, usize)> = g.edges.iter().map(|edge| (edge.from, edge.to)).collect();
        assert!(ends.iter().any(|&(_, to)| to == anchor), "{ends:?}");
        assert!(ends.iter().any(|&(from, _)| from == anchor), "{ends:?}");
    }

    #[test]
    fn a_composite_of_composites_stands_alone_when_entered() {
        let g = graph("state App {\n state Conn {\n  A --> B\n }\n}\n[*] --> App");
        assert_eq!(member_labels(&g, subgraph(&g, "Conn")), ["A", "B"]);
        assert_eq!(g.subgraphs.len(), 1);
        assert!(labels(&g).contains(&"App".to_owned()));
    }

    #[test]
    fn front_matter_is_skipped() {
        let g = parse(
            "---\nconfig:\n  state:\n    useMaxWidth: false\n---\nstateDiagram-v2\n  A --> B",
        )
        .expect("fixture should parse");
        assert_eq!(labels(&g), ["A", "B"]);
    }

    #[test_case("state F <<fork>>", ParseError::Unsupported("state F <<fork>>".to_owned()) ; "fork")]
    #[test_case("A --> B --> C", ParseError::Unsupported("A --> B --> C".to_owned())      ; "chained")]
    #[test_case("--", ParseError::Unsupported("--".to_owned())                            ; "region_outside_composite")]
    #[test_case("direction SIDEWAYS", ParseError::Unsupported("direction SIDEWAYS".to_owned()) ; "unknown_direction")]
    #[test_case("A <-- B", ParseError::Unsupported("A <-- B".to_owned())                  ; "reverse_arrow")]
    #[test_case("}", ParseError::UnmatchedBrace                                           ; "unmatched_brace")]
    #[test_case("state C {\n A --> B", ParseError::UnterminatedComposite                  ; "unterminated_composite")]
    #[test_case("note left of A\n text", ParseError::UnterminatedNote                     ; "unterminated_note")]
    #[test_case("%% only a comment", ParseError::Empty                                    ; "empty")]
    fn unsupported_state_syntax_is_refused(body: &str, expected: ParseError) {
        assert_eq!(parse(&format!("stateDiagram-v2\n{body}")), Err(expected));
    }
}
