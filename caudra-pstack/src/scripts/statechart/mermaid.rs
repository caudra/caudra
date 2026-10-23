//! Reading a Mermaid `stateDiagram-v2` back as the machine it draws: upstream's `parseMermaid`,
//! `productOf` and `compareCharts`. The regions chart proves itself with them, and the chart
//! comparison reads an agreed chart with them.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

use super::ordered::{OrderedMap, OrderedSet};
use super::text::{LEGEND_ID, collapse_spaces};

const START: &str = "[*]";
const MAIN: &str = "Main";
const JOIN: &str = " | ";
const OR: &str = " or ";
const SEP: char = '\0';

const UNREACHED: &str = "states the agreed chart's own steps never reach";
const ONLY_EXPECTED_STATES: &str = "states only in EXPECTED (no path from the start reaches them in the spec, or the spec renamed them)";
const ONLY_ACTUAL_STATES: &str = "states only in ACTUAL";
const ONLY_EXPECTED_STARTS: &str = "start states only in EXPECTED";
const ONLY_ACTUAL_STARTS: &str = "start states only in ACTUAL";
const ONLY_EXPECTED_STEPS: &str =
    "transitions only in EXPECTED (a guard in the spec blocks them, or the spec renamed the step)";
const ONLY_ACTUAL_STEPS: &str = "transitions only in ACTUAL";

static FRONT_MATTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*---[ \t]*\r?\n(?s:.*?)\r?\n---[ \t]*(?:\r?\n|$)").expect("valid regex")
});
static CLASS_SHORTHAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\[\*\]|[A-Za-z_][A-Za-z0-9_]*):::[A-Za-z0-9_-]+").expect("valid regex")
});
static WHEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^%%\s*when\s+(not\s+)?(.+)$").expect("valid regex"));
static NEVER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^%%\s*never\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.+?)(?:\s+--\s+(.*))?$")
        .expect("valid regex")
});
static SKIPPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:stateDiagram|direction|classDef|class |note)").expect("valid regex")
});
static NAMED_STATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^state\s+"([^"]*)"\s+as\s+([A-Za-z_][A-Za-z0-9_]*)\s*(\{)?\s*$"#)
        .expect("valid regex")
});
static PLAIN_STATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^state\s+([A-Za-z_][A-Za-z0-9_]*)\s*(\{)?\s*$").expect("valid regex")
});
static TRANSITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\[\*\]|[A-Za-z_][A-Za-z0-9_]*)\s*-->\s*(\[\*\]|[A-Za-z_][A-Za-z0-9_]*)(?:\s*:\s*(.*))?$",
    )
    .expect("valid regex")
});
static DESCRIPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$").expect("valid regex"));
static GUARD_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\[[^\]]*\]").expect("valid regex"));
static BREAKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<br\s*/?>|#160;").expect("valid regex"));
static QUOTES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#(?:quot|34);").expect("valid regex"));

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MermaidError {
    #[error("a %% when line must come right after a transition: {0}")]
    StrayWhen(String),
    #[error("the guarded step \"{0}\" must end its label with the guard's words in [ ]")]
    UnwordedGuard(String),
    #[error("cannot read final state in \"{0}\"; draw the step to the state the reader sees next")]
    FinalState(String),
    #[error("unmatched }} in the chart")]
    UnmatchedClose,
    #[error("cannot read chart line: {0}")]
    Unreadable(String),
    #[error("the chart has an unclosed {{")]
    Unclosed,
    #[error(
        "the chart has -- outside a composite; to make parallel regions, split one top-level composite with --"
    )]
    TopLevelSplit,
    #[error("transition names {0}, which is not in this region")]
    Elsewhere(String),
    #[error("state id {0} is declared in two places")]
    Twice(String),
    #[error("two states have the caption \"{0}\"; give each its own caption")]
    SameCaption(String),
    #[error("{0} splits into regions below the top level; make each region a top-level region")]
    NestedSplit(String),
    #[error("{0} has no [*] --> start, so a transition into it has nowhere to land")]
    NoStart(String),
    #[error("{0} splits into regions below the top level")]
    NestedRegions(String),
    #[error("a %% when line guards a step on other regions, and the chart has none")]
    GuardWithoutRegions,
    #[error("a %% when line names {0}, which is not a state id in the chart")]
    UnknownGuardState(String),
    #[error(
        "a %% when line names {0}, which is in the step's own region; a guard reads other regions"
    )]
    OwnRegionGuard(String),
}

/// A step's condition on the other regions: it may happen only while each named region is in one
/// of its states, or, with `not`, while none is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub not: bool,
    pub states: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: String,
    pub to: String,
    pub label: String,
    pub guard: Option<Guard>,
}

#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub states: OrderedSet<String>,
    pub initial: OrderedSet<String>,
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Never {
    pub name: String,
    pub states: Vec<String>,
    pub sentence: String,
}

#[derive(Debug)]
pub struct Chart {
    /// Each region by name, in the chart's order: one, `Main`, when the chart has no regions.
    pub regions: Vec<(String, Machine)>,
    pub nevers: Vec<Never>,
}

#[derive(Debug, Clone)]
struct When {
    not: bool,
    ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct Drawn {
    from: String,
    to: String,
    label: String,
    words: bool,
    when: Option<When>,
}

#[derive(Debug, Default)]
struct Part {
    children: Vec<usize>,
    transitions: Vec<usize>,
}

#[derive(Debug)]
struct Node {
    name: String,
    leaf: bool,
    parts: Vec<Part>,
}

struct Info {
    all: Vec<String>,
    initial: Vec<String>,
    depth: usize,
}

struct Tree {
    nodes: Vec<Node>,
    drawn: Vec<Drawn>,
    names: HashMap<String, String>,
}

impl Tree {
    fn display(&self, id: &str) -> String {
        self.names.get(id).cloned().unwrap_or_else(|| id.to_owned())
    }

    fn add(&mut self, name: &str, leaf: bool) -> usize {
        self.nodes.push(Node {
            name: name.to_owned(),
            leaf,
            parts: if leaf {
                Vec::new()
            } else {
                vec![Part::default()]
            },
        });
        self.nodes.len() - 1
    }

    fn part(&mut self, (node, part): (usize, usize)) -> &mut Part {
        &mut self.nodes[node].parts[part]
    }
}

/// Reads a chart as upstream's `parseMermaid` does.
pub fn parse(text: &str) -> Result<Chart, MermaidError> {
    let mut tree = Tree {
        nodes: Vec::new(),
        drawn: Vec::new(),
        names: HashMap::new(),
    };
    let root = tree.add("", false);
    let mut stack = vec![root];
    let mut implicit: HashMap<String, Option<(usize, usize)>> = HashMap::new();
    let mut nevers = Vec::new();
    let mut last: Option<usize> = None;
    let current = |tree: &Tree, stack: &[usize]| {
        let node = stack[stack.len() - 1];
        (node, tree.nodes[node].parts.len() - 1)
    };
    let body = FRONT_MATTER.replace(text, "");
    for raw in body.split('\n') {
        let line = CLASS_SHORTHAND.replace_all(raw.trim(), "$1");
        let line = line.as_ref();
        if let Some(when) = WHEN.captures(line) {
            let at = last.ok_or_else(|| MermaidError::StrayWhen(line.to_owned()))?;
            if !tree.drawn[at].words {
                return Err(MermaidError::UnwordedGuard(tree.drawn[at].label.clone()));
            }
            tree.drawn[at].when = Some(When {
                not: when.get(1).is_some(),
                ids: when[2].split(',').map(|id| id.trim().to_owned()).collect(),
            });
            last = None;
            continue;
        }
        last = None;
        if let Some(never) = NEVER.captures(line) {
            nevers.push(Never {
                name: never[1].to_owned(),
                states: never[2].split('&').map(|s| s.trim().to_owned()).collect(),
                sentence: never.get(3).map_or("", |s| s.as_str()).to_owned(),
            });
            continue;
        }
        if line.is_empty() || line.starts_with("%%") || SKIPPED.is_match(line) {
            continue;
        }
        let declared = NAMED_STATE
            .captures(line)
            .map(|m| (Some(m[1].to_owned()), m[2].to_owned(), m.get(3).is_some()))
            .or_else(|| {
                PLAIN_STATE
                    .captures(line)
                    .map(|m| (None, m[1].to_owned(), m.get(2).is_some()))
            });
        if let Some((caption, id, opens)) = declared {
            if let Some(caption) = caption {
                tree.names
                    .entry(id.clone())
                    .or_insert_with(|| unwrap(&caption));
            }
            declare(&mut tree, &mut implicit, &id);
            let at = current(&tree, &stack);
            let node = tree.add(&id, !opens);
            tree.part(at).children.push(node);
            if opens {
                stack.push(node);
            }
            continue;
        }
        if line == "}" {
            if stack.len() == 1 {
                return Err(MermaidError::UnmatchedClose);
            }
            stack.pop();
            continue;
        }
        if line == "--" {
            let top = stack[stack.len() - 1];
            tree.nodes[top].parts.push(Part::default());
            continue;
        }
        if let Some(step) = TRANSITION.captures(line) {
            let (from, to) = (step[1].to_owned(), step[2].to_owned());
            if to == START && from != START {
                return Err(MermaidError::FinalState(line.to_owned()));
            }
            for end in [&from, &to] {
                let at = current(&tree, &stack);
                mention(&mut tree, &mut implicit, at, end);
            }
            let said = step.get(3).map_or_else(String::new, |s| unwrap(s.as_str()));
            let label = GUARD_WORDS.replace_all(&said, "").into_owned();
            let words = label != said;
            tree.drawn.push(Drawn {
                from,
                to,
                label,
                words,
                when: None,
            });
            let at = current(&tree, &stack);
            let index = tree.drawn.len() - 1;
            tree.part(at).transitions.push(index);
            last = Some(index);
            continue;
        }
        if let Some(described) = DESCRIPTION.captures(line) {
            let id = described[1].to_owned();
            tree.names
                .entry(id.clone())
                .or_insert_with(|| unwrap(&described[2]));
            if implicit.get(&id) == Some(&None) {
                continue;
            }
            declare(&mut tree, &mut implicit, &id);
            let at = current(&tree, &stack);
            let node = tree.add(&id, true);
            tree.part(at).children.push(node);
            continue;
        }
        return Err(MermaidError::Unreadable(line.to_owned()));
    }
    if stack.len() != 1 {
        return Err(MermaidError::Unclosed);
    }
    if tree.nodes[root].parts.len() > 1 {
        return Err(MermaidError::TopLevelSplit);
    }
    let regions = regions_of(&tree, root)?;
    Ok(Chart { regions, nevers })
}

fn declare(tree: &mut Tree, implicit: &mut HashMap<String, Option<(usize, usize)>>, id: &str) {
    if let Some(Some(at)) = implicit.get(id).cloned() {
        let children = &tree.nodes[at.0].parts[at.1].children;
        if let Some(found) = children
            .iter()
            .position(|&child| tree.nodes[child].name == id)
        {
            tree.part(at).children.remove(found);
        }
    }
    implicit.insert(id.to_owned(), None);
}

fn mention(
    tree: &mut Tree,
    implicit: &mut HashMap<String, Option<(usize, usize)>>,
    at: (usize, usize),
    id: &str,
) {
    if id == START || implicit.contains_key(id) {
        return;
    }
    let node = tree.add(id, true);
    tree.part(at).children.push(node);
    implicit.insert(id.to_owned(), Some(at));
}

fn unwrap(text: &str) -> String {
    let spaced = BREAKS.replace_all(text, " ");
    collapse_spaces(&QUOTES.replace_all(&spaced, "\""))
}

fn regions_of(tree: &Tree, root: usize) -> Result<Vec<(String, Machine)>, MermaidError> {
    let top = &tree.nodes[root].parts[0];
    let machine: Vec<usize> = top
        .children
        .iter()
        .copied()
        .filter(|&child| tree.nodes[child].name != LEGEND_ID)
        .collect();
    let parallel = machine.len() == 1
        && !tree.nodes[machine[0]].leaf
        && tree.nodes[machine[0]].parts.len() > 1
        && top.transitions.iter().all(|&t| tree.drawn[t].from == START);
    let mut read = Regions::default();
    if parallel {
        for part in &tree.nodes[machine[0]].parts {
            let holder = match part.children.as_slice() {
                [only]
                    if !tree.nodes[*only].leaf
                        && part.transitions.iter().all(|&t| {
                            tree.drawn[t].from == START
                                && tree.drawn[t].to == tree.nodes[*only].name
                        }) =>
                {
                    Some(*only)
                }
                _ => None,
            };
            if let Some(holder) = holder
                && tree.nodes[holder].parts.len() > 1
            {
                return Err(MermaidError::NestedRegions(
                    tree.display(&tree.nodes[holder].name),
                ));
            }
            let name = match holder {
                Some(holder) => tree.display(&tree.nodes[holder].name),
                None => format!("Region{}", read.regions.len() + 1),
            };
            read.put(
                name,
                region(tree, holder.map_or(part, |h| &tree.nodes[h].parts[0]))?,
            );
        }
    } else {
        read.put(MAIN.to_owned(), region(tree, top)?);
    }
    let Regions {
        mut regions,
        owners,
        whens,
    } = read;
    for ((name, machine), when) in regions.iter_mut().zip(whens) {
        for (transition, when) in machine.transitions.iter_mut().zip(when) {
            let Some(when) = when else {
                continue;
            };
            if !parallel {
                return Err(MermaidError::GuardWithoutRegions);
            }
            let mut states: Vec<(String, Vec<String>)> = Vec::new();
            for id in &when.ids {
                let (owner, all) = owners
                    .get(id)
                    .ok_or_else(|| MermaidError::UnknownGuardState(id.clone()))?;
                if owner == name {
                    return Err(MermaidError::OwnRegionGuard(tree.display(id)));
                }
                let at = match states.iter().position(|(known, _)| known == owner) {
                    Some(at) => at,
                    None => {
                        states.push((owner.clone(), Vec::new()));
                        states.len() - 1
                    }
                };
                let merged = &mut states[at].1;
                merged.extend(all.iter().cloned());
                merged.sort();
                merged.dedup();
            }
            transition.guard = Some(Guard {
                not: when.not,
                states,
            });
        }
    }
    Ok(regions)
}

type Region = (Machine, HashMap<String, Info>, Vec<Option<When>>);

#[derive(Default)]
struct Regions {
    regions: Vec<(String, Machine)>,
    owners: HashMap<String, (String, Vec<String>)>,
    whens: Vec<Vec<Option<When>>>,
}

impl Regions {
    fn put(&mut self, name: String, (machine, info, when): Region) {
        for (id, Info { all, .. }) in info {
            self.owners.insert(id, (name.clone(), all));
        }
        match self.regions.iter().position(|(known, _)| *known == name) {
            Some(at) => {
                self.regions[at].1 = machine;
                self.whens[at] = when;
            }
            None => {
                self.regions.push((name, machine));
                self.whens.push(when);
            }
        }
    }
}

fn region(tree: &Tree, part: &Part) -> Result<Region, MermaidError> {
    let mut reading = Reading {
        tree,
        out: Machine::default(),
        info: HashMap::new(),
        declared: HashSet::new(),
        transitions: Vec::new(),
    };
    reading.collect(part);
    reading.states(part, "", 0)?;
    for &t in &part.transitions {
        if tree.drawn[t].from == START {
            let initial = reading.lookup(&tree.drawn[t].to)?.initial.clone();
            reading.out.initial.extend(initial);
        }
    }
    let mut ordered: Vec<(usize, usize)> = reading
        .transitions
        .iter()
        .map(|&t| Ok((reading.lookup(&tree.drawn[t].from)?.depth, t)))
        .collect::<Result<_, MermaidError>>()?;
    ordered.sort_by_key(|&(depth, _)| Reverse(depth));
    let mut handled: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = Vec::new();
    let mut depth = None;
    let mut whens = Vec::new();
    for (from_depth, t) in ordered {
        let drawn = &tree.drawn[t];
        if depth != Some(from_depth) {
            handled.extend(pending.drain(..));
            depth = Some(from_depth);
        }
        let from_all = reading.lookup(&drawn.from)?.all.clone();
        let to_initial = reading.lookup(&drawn.to)?.initial.clone();
        for from in from_all {
            let key = format!("{from}{SEP}{}", drawn.label);
            if !drawn.label.is_empty() && handled.contains(&key) {
                continue;
            }
            for to in &to_initial {
                reading.out.transitions.push(Transition {
                    from: from.clone(),
                    to: to.clone(),
                    label: drawn.label.clone(),
                    guard: None,
                });
                whens.push(drawn.when.clone());
            }
            if !drawn.label.is_empty() {
                pending.push(key);
            }
        }
    }
    Ok((reading.out, reading.info, whens))
}

struct Reading<'a> {
    tree: &'a Tree,
    out: Machine,
    info: HashMap<String, Info>,
    declared: HashSet<String>,
    transitions: Vec<usize>,
}

impl Reading<'_> {
    fn collect(&mut self, part: &Part) {
        for &child in &part.children {
            let node = &self.tree.nodes[child];
            if node.name == LEGEND_ID {
                continue;
            }
            self.declared.insert(node.name.clone());
            if !node.leaf {
                for inner in &node.parts {
                    self.collect(inner);
                }
            }
        }
    }

    fn lookup(&self, id: &str) -> Result<&Info, MermaidError> {
        self.info
            .get(id)
            .ok_or_else(|| MermaidError::Elsewhere(id.to_owned()))
    }

    fn states(
        &mut self,
        part: &Part,
        prefix: &str,
        depth: usize,
    ) -> Result<Vec<String>, MermaidError> {
        let tree = self.tree;
        let mut local: Vec<(&str, Option<usize>)> = part
            .children
            .iter()
            .filter(|&&child| tree.nodes[child].name != LEGEND_ID)
            .map(|&child| (tree.nodes[child].name.as_str(), Some(child)))
            .collect();
        for &t in &part.transitions {
            for end in [&tree.drawn[t].from, &tree.drawn[t].to] {
                if end == START || self.declared.contains(end) {
                    continue;
                }
                self.declared.insert(end.clone());
                local.push((end, None));
            }
        }
        let mut all = Vec::new();
        for (at, &(name, node)) in local.iter().enumerate() {
            if self.info.contains_key(name) {
                if local.iter().position(|(other, _)| *other == name) != Some(at) {
                    continue;
                }
                return Err(MermaidError::Twice(name.to_owned()));
            }
            let full = format!("{prefix}{}", tree.display(name));
            let composite = node.filter(|&node| !tree.nodes[node].leaf);
            let Some(composite) = composite else {
                if self.out.states.contains(&full) {
                    return Err(MermaidError::SameCaption(full));
                }
                self.out.states.insert(full.clone());
                self.info.insert(
                    name.to_owned(),
                    Info {
                        all: vec![full.clone()],
                        initial: vec![full.clone()],
                        depth,
                    },
                );
                all.push(full);
                continue;
            };
            let parts = &tree.nodes[composite].parts;
            if parts.len() > 1 {
                return Err(MermaidError::NestedSplit(tree.display(name)));
            }
            let inner = self.states(&parts[0], &format!("{full}."), depth + 1)?;
            let mut initial = Vec::new();
            for &t in &parts[0].transitions {
                if tree.drawn[t].from == START {
                    initial.extend(self.lookup(&tree.drawn[t].to)?.initial.iter().cloned());
                }
            }
            if initial.is_empty() {
                return Err(MermaidError::NoStart(tree.display(name)));
            }
            all.extend(inner.iter().cloned());
            self.info.insert(
                name.to_owned(),
                Info {
                    all: inner,
                    initial,
                    depth,
                },
            );
        }
        for &t in &part.transitions {
            if tree.drawn[t].from != START && tree.drawn[t].to != START {
                self.transitions.push(t);
            }
        }
        Ok(all)
    }
}

fn guard_holds(guard: Option<&Guard>, at: &[(&str, &str)]) -> bool {
    let Some(guard) = guard else {
        return true;
    };
    let mut hits = guard.states.iter().map(|(region, states)| {
        at.iter()
            .find(|(name, _)| name == region)
            .is_some_and(|(_, state)| states.iter().any(|s| s == state))
    });
    if guard.not {
        !hits.any(|hit| hit)
    } else {
        hits.all(|hit| hit)
    }
}

struct Move<'a> {
    to: &'a str,
    guard: Option<&'a Guard>,
}

/// The one machine that the regions make together, from their start states: states read
/// "a | b", and a label moves every region that has a step with it at once.
pub fn product_of<'r>(regions: &'r [(String, Machine)]) -> Machine {
    let names: Vec<&str> = regions.iter().map(|(name, _)| name.as_str()).collect();
    let mut by_label: OrderedMap<&str, Vec<HashMap<&str, Vec<Move>>>> = OrderedMap::default();
    let mut unlabeled = Vec::new();
    for (index, (_, region)) in regions.iter().enumerate() {
        for t in &region.transitions {
            if t.label.is_empty() {
                unlabeled.push((index, t));
                continue;
            }
            let moves = by_label.entry(&t.label, || {
                regions.iter().map(|_| HashMap::new()).collect()
            });
            moves[index].entry(t.from.as_str()).or_default().push(Move {
                to: &t.to,
                guard: t.guard.as_ref(),
            });
        }
    }
    let mut starts: Vec<Vec<&str>> = vec![Vec::new()];
    for (_, region) in regions {
        starts = starts
            .iter()
            .flat_map(|prefix| {
                region.initial.iter().map(move |state| {
                    let mut next = prefix.clone();
                    next.push(state.as_str());
                    next
                })
            })
            .collect();
    }
    let mut out = Machine {
        initial: starts.iter().map(|parts| parts.join(JOIN)).collect(),
        ..Machine::default()
    };
    let mut seen: HashSet<String> = starts.iter().map(|parts| parts.join(JOIN)).collect();
    let mut queue: VecDeque<Vec<&str>> = starts.into_iter().collect();
    while let Some(current) = queue.pop_front() {
        let here = current.join(JOIN);
        out.states.insert(here.clone());
        let at: Vec<(&str, &str)> = names.iter().copied().zip(current.iter().copied()).collect();
        let mut step = |next: Vec<&'r str>, label: &str| {
            let there = next.join(JOIN);
            if here == there {
                return;
            }
            out.transitions.push(Transition {
                from: here.clone(),
                to: there.clone(),
                label: label.to_owned(),
                guard: None,
            });
            if seen.insert(there) {
                queue.push_back(next);
            }
        };
        for (index, t) in &unlabeled {
            if current[*index] != t.from || !guard_holds(t.guard.as_ref(), &at) {
                continue;
            }
            let mut next = current.clone();
            next[*index] = &t.to;
            step(next, "");
        }
        for (label, per_region) in by_label.iter() {
            let open: Vec<Vec<&str>> = per_region
                .iter()
                .enumerate()
                .map(|(index, moves)| {
                    moves.get(current[index]).map_or_else(Vec::new, |moves| {
                        moves
                            .iter()
                            .filter(|m| guard_holds(m.guard, &at))
                            .map(|m| m.to)
                            .collect()
                    })
                })
                .collect();
            if open.iter().all(Vec::is_empty) {
                continue;
            }
            let mut nexts: Vec<Vec<&str>> = vec![Vec::new()];
            for (index, tos) in open.iter().enumerate() {
                let options: Vec<&str> = if tos.is_empty() {
                    vec![current[index]]
                } else {
                    tos.iter()
                        .copied()
                        .collect::<OrderedSet<_>>()
                        .iter()
                        .copied()
                        .collect()
                };
                nexts = nexts
                    .iter()
                    .flat_map(|prefix| {
                        options.iter().map(move |state| {
                            let mut next = prefix.clone();
                            next.push(*state);
                            next
                        })
                    })
                    .collect();
            }
            for next in nexts {
                step(next, label);
            }
        }
    }
    out
}

/// What differs between the machines two charts draw, by kind, each sorted; empty when they draw
/// the same machine.
pub fn compare(
    expected: &[(String, Machine)],
    actual: &[(String, Machine)],
) -> Vec<(&'static str, Vec<String>)> {
    let wanted = product_of(expected);
    let made = product_of(actual);
    let mut unreached = Vec::new();
    if let [(_, only)] = expected {
        unreached.extend(
            only.states
                .iter()
                .filter(|s| !wanted.states.contains(s))
                .cloned(),
        );
    } else {
        for (index, (name, region)) in expected.iter().enumerate() {
            let seen: HashSet<&str> = wanted
                .states
                .iter()
                .filter_map(|state| state.split(JOIN).nth(index))
                .collect();
            unreached.extend(
                region
                    .states
                    .iter()
                    .filter(|s| !seen.contains(s.as_str()))
                    .map(|s| format!("{name}: {s}")),
            );
        }
    }
    unreached.sort();
    let (wanted_steps, made_steps) = (step_set(&wanted), step_set(&made));
    [
        (UNREACHED, unreached),
        (ONLY_EXPECTED_STATES, minus(&wanted.states, &made.states)),
        (ONLY_ACTUAL_STATES, minus(&made.states, &wanted.states)),
        (ONLY_EXPECTED_STARTS, minus(&wanted.initial, &made.initial)),
        (ONLY_ACTUAL_STARTS, minus(&made.initial, &wanted.initial)),
        (ONLY_EXPECTED_STEPS, minus(&wanted_steps, &made_steps)),
        (ONLY_ACTUAL_STEPS, minus(&made_steps, &wanted_steps)),
    ]
    .into_iter()
    .filter(|(_, differences)| !differences.is_empty())
    .collect()
}

fn step_set(machine: &Machine) -> OrderedSet<String> {
    let mut labeled = HashSet::new();
    let mut out = OrderedSet::default();
    for t in machine.transitions.iter().filter(|t| !t.label.is_empty()) {
        labeled.insert(format!("{} --> {}", t.from, t.to));
        for label in t.label.split(OR) {
            out.insert(format!("{} --> {} : {label}", t.from, t.to));
        }
    }
    for t in machine.transitions.iter().filter(|t| t.label.is_empty()) {
        let bare = format!("{} --> {}", t.from, t.to);
        if !labeled.contains(&bare) {
            out.insert(bare);
        }
    }
    out
}

fn minus(a: &OrderedSet<String>, b: &OrderedSet<String>) -> Vec<String> {
    let mut only: Vec<String> = a.iter().filter(|x| !b.contains(x)).cloned().collect();
    only.sort();
    only
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN_REGION: &str = "Main";

    fn edges(machine: &Machine) -> Vec<String> {
        let mut edges: Vec<String> = machine
            .transitions
            .iter()
            .map(|t| format!("{} -> {} : {}", t.from, t.to, t.label))
            .collect();
        edges.sort();
        edges
    }

    fn main_region(text: &str) -> Machine {
        let chart = parse(text).expect("the chart should parse");
        let [(name, machine)] = <[_; 1]>::try_from(chart.regions).expect("one region");
        assert_eq!(name, MAIN_REGION);
        machine
    }

    #[test]
    fn a_transition_on_a_composite_leaves_from_every_state_inside_it() {
        let machine = main_region(
            "stateDiagram-v2\n  [*] --> Busy\n  state Busy {\n    [*] --> A\n    A --> B : next\n  }\n  Busy --> Done : cancel\n",
        );

        assert_eq!(
            edges(&machine),
            [
                "Busy.A -> Busy.B : next",
                "Busy.A -> Done : cancel",
                "Busy.B -> Done : cancel",
            ]
        );
        assert_eq!(machine.initial.iter().collect::<Vec<_>>(), ["Busy.A"]);
    }

    #[test]
    fn the_deepest_source_wins_when_a_composite_and_a_state_inside_it_share_a_label() {
        let machine = main_region(
            "stateDiagram-v2\n  [*] --> Busy\n  state Busy {\n    [*] --> A\n    A --> B : go\n  }\n  Busy --> Done : go\n",
        );

        assert_eq!(
            edges(&machine),
            ["Busy.A -> Busy.B : go", "Busy.B -> Done : go"]
        );
    }

    #[test]
    fn a_top_level_composite_split_by_dashes_makes_one_region_per_part() {
        let chart = parse(
            "stateDiagram-v2\n  state Both {\n    [*] --> Off\n    Off --> On : flip\n    --\n    [*] --> Quiet\n    Quiet --> Loud : shout\n  }\n",
        )
        .expect("parses");

        let names: Vec<&str> = chart
            .regions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["Region1", "Region2"]);
        let product = product_of(&chart.regions);
        assert_eq!(product.states.len(), 4);
    }

    #[test]
    fn a_when_line_guards_a_step_on_the_other_regions_states() {
        let chart = parse(
            "stateDiagram-v2\n  state Lamp {\n    state \"Power\" as region0 {\n      [*] --> p0\n      state \"Off\" as p0\n      state \"On\" as p1\n      p0 --> p1 : switch\n    }\n    --\n    state \"Light\" as region1 {\n      [*] --> l0\n      state \"Dark\" as l0\n      state \"Lit\" as l1\n      l0 --> l1 : glow [when On]\n      %% when p1\n    }\n  }\n",
        )
        .expect("parses");

        let product = product_of(&chart.regions);
        assert_eq!(
            edges(&product),
            [
                "Off | Dark -> On | Dark : switch",
                "On | Dark -> On | Lit : glow"
            ]
        );
    }

    #[test]
    fn the_legend_is_not_part_of_the_machine() {
        let machine = main_region(
            "stateDiagram-v2\n  state \"Legend\" as legend {\n    state \"Works\" as legendWorks\n  }\n  [*] --> A\n  A --> B : go\n",
        );

        assert_eq!(machine.states.iter().collect::<Vec<_>>(), ["A", "B"]);
    }

    #[test]
    fn the_same_machine_compares_equal_and_a_missing_step_shows() {
        let full = parse("stateDiagram-v2\n  [*] --> A\n  A --> B : go or run\n").expect("parses");
        let split = parse("stateDiagram-v2\n  [*] --> A\n  A --> B : go\n  A --> B : run\n")
            .expect("parses");
        let less = parse("stateDiagram-v2\n  [*] --> A\n  A --> B : go\n").expect("parses");

        assert!(compare(&full.regions, &split.regions).is_empty());
        assert_eq!(
            compare(&full.regions, &less.regions),
            [(ONLY_EXPECTED_STEPS, vec!["A --> B : run".to_owned()])]
        );
    }

    #[test]
    fn front_matter_and_quotes_read_back() {
        let machine = main_region(
            "---\nconfig:\n  state:\n    useMaxWidth: false\n---\nstateDiagram-v2\n  state \"Badge says #quot;Synced#quot;\" as s0\n  [*] --> s0\n",
        );

        assert_eq!(
            machine.states.iter().collect::<Vec<_>>(),
            ["Badge says \"Synced\""]
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert_eq!(
            parse("stateDiagram-v2\n  A --> [*]\n").unwrap_err(),
            MermaidError::FinalState("A --> [*]".to_owned())
        );
        assert_eq!(
            parse("stateDiagram-v2\n  }\n").unwrap_err(),
            MermaidError::UnmatchedClose
        );
        assert_eq!(
            parse("stateDiagram-v2\n  state A {\n").unwrap_err(),
            MermaidError::Unclosed
        );
        assert_eq!(
            parse("stateDiagram-v2\n  ???\n").unwrap_err(),
            MermaidError::Unreadable("???".to_owned())
        );
    }
}
