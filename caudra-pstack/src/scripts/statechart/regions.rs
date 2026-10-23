//! Upstream's `regionsChart`: when every reader state reads "context: progress", the chart can
//! draw two parallel regions, contexts and progress, in place of their product. It does so only
//! when they are smaller, keep each state's status, and combine back into exactly the flat
//! chart's steps, and then it draws only the arrows the bug paths, the shortest way to each region
//! state, and the nearest step with each label need, plus any arrow that keeps the drawn ones from
//! making a step the flat chart lacks.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::chart::{
    Chart, ChartError, DrawnState, Edge, Status, front_matter, guarded, lines_text,
};
use super::mermaid::{Machine, Transition, compare, parse, product_of};
use super::ordered::{OrderedMap, OrderedSet};
use super::text::{STATE_WIDTH, STEP_WIDTH, collapse_spaces, escape_quotes, locale_cmp, wrap};

const CONTEXT: &str = ": ";
const JOIN: &str = " | ";
const SEP: char = '\0';
const MAIN: &str = "Main";
const OR: &str = " or ";
const SIDES: [usize; 2] = [0, 1];
/// The order a region state takes the best status of the flat states it is part of.
const RANK: [Status; 3] = [Status::Works, Status::Fixed, Status::Wrong];
const NO_LABEL: &str = "a step has no label";

type Parts = [String; 2];

/// What the regions drew, or why they did not, with the flat chart the proof compares with.
pub(super) struct RegionsChart {
    pub(super) proof: Option<String>,
    pub(super) drawn: Result<RegionsDrawn, String>,
}

pub(super) struct RegionsDrawn {
    pub(super) mermaid: String,
    pub(super) all_mermaid: String,
    pub(super) states: [usize; 2],
    pub(super) transitions: usize,
    pub(super) all: usize,
    pub(super) flat_states: usize,
    pub(super) flat_transitions: usize,
    /// Region states per status, in `RANK` order.
    pub(super) status: Vec<(Status, usize)>,
    pub(super) groups: Vec<(String, String, usize)>,
}

#[derive(Debug, Clone)]
struct FlatStep {
    from: Parts,
    to: Parts,
    label: String,
}

impl FlatStep {
    fn key(&self) -> String {
        step_key(&bar(&self.from), &self.label, &bar(&self.to))
    }

    /// The region moves this step makes: one per region whose state it changes.
    fn moves(&self) -> Vec<(usize, String)> {
        SIDES
            .into_iter()
            .filter(|&side| self.from[side] != self.to[side])
            .map(|side| (side, move_id(&self.from[side], &self.label, &self.to[side])))
            .collect()
    }
}

struct Move {
    from: String,
    label: String,
    to: String,
    beside: OrderedSet<String>,
    words: BTreeSet<String>,
}

enum Target {
    At(usize, String),
    Ends(String),
}

fn norm(text: &str) -> String {
    collapse_spaces(&text.replace(';', ","))
}

fn split(key: &str) -> Option<Parts> {
    let at = key.find(CONTEXT)?;
    (at > 0 && at + CONTEXT.len() < key.len())
        .then(|| [norm(&key[..at]), norm(&key[at + CONTEXT.len()..])])
}

fn bar(parts: &Parts) -> String {
    parts.join(JOIN)
}

fn move_id(from: &str, label: &str, to: &str) -> String {
    format!("{from}{SEP}{label}{SEP}{to}")
}

fn step_key(from: &str, label: &str, to: &str) -> String {
    format!("{from} --> {to} : {label}")
}

struct Regions<'r, 'c> {
    chart: &'r Chart<'c>,
    nodes: usize,
    beside: [OrderedMap<String, OrderedSet<String>>; 2],
    moves: [OrderedMap<String, Move>; 2],
    ids: [OrderedMap<String, String>; 2],
    drawn: [Vec<DrawnState>; 2],
    starts: Vec<Parts>,
}

impl Regions<'_, '_> {
    fn draw(&self, only: Option<&[HashSet<String>; 2]>) -> (String, usize) {
        let mut out = self.chart.head(&self.drawn.concat());
        out.push("  state \"At the same time\" as regions {".to_owned());
        let mut count = 0;
        let names = self.chart.charter.mapping.region_names.as_ref();
        for side in SIDES {
            if side == 1 {
                out.push("    --".to_owned());
            }
            let name = names.map(|names| &names[side]);
            let pad = if name.is_some() { "      " } else { "    " };
            if let Some(name) = name {
                out.push(format!(
                    "    state \"{}\" as region{side} {{",
                    escape_quotes(&wrap(name, STATE_WIDTH))
                ));
            }
            let starts: OrderedSet<&String> =
                self.starts.iter().map(|parts| &parts[side]).collect();
            for start in starts.iter() {
                out.push(format!("{pad}[*] --> {}", self.id(side, start)));
            }
            for (state, id) in self.ids[side].iter() {
                out.push(format!(
                    "{pad}state \"{}\" as {id}",
                    escape_quotes(&wrap(state, STATE_WIDTH))
                ));
            }
            let mut shown: Vec<&Move> = self.moves[side]
                .iter()
                .filter(|(id, _)| only.is_none_or(|only| only[side].contains(*id)))
                .map(|(_, m)| m)
                .collect();
            shown.sort_by(|a, b| {
                locale_cmp(&a.from, &b.from).then_with(|| locale_cmp(&a.label, &b.label))
            });
            for m in shown {
                let all = self.beside[side].get(&m.from);
                let mut not: Vec<&String> = all
                    .into_iter()
                    .flat_map(OrderedSet::iter)
                    .filter(|s| !m.beside.contains(s))
                    .collect();
                not.sort();
                let mut yes: Vec<&String> = m.beside.iter().collect();
                yes.sort();
                let unless = !not.is_empty() && not.len() < yes.len();
                let guard = match (not.is_empty(), unless) {
                    (true, _) => None,
                    (false, true) => Some(not),
                    (false, false) => Some(yes),
                };
                let worded = guarded(Some(&m.words), &m.label);
                let text = match &guard {
                    Some(states) if worded == m.label => format!(
                        "{} [{} {}]",
                        m.label,
                        if unless { "unless" } else { "when" },
                        states
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(OR)
                    ),
                    _ => worded,
                };
                out.push(format!(
                    "{pad}{} --> {} : {}",
                    self.id(side, &m.from),
                    self.id(side, &m.to),
                    wrap(&text, STEP_WIDTH)
                ));
                if let Some(states) = guard {
                    let ids: Vec<&str> = states.iter().map(|s| self.id(1 - side, s)).collect();
                    out.push(format!(
                        "{pad}%% when {}{}",
                        if unless { "not " } else { "" },
                        ids.join(", ")
                    ));
                }
                count += 1;
            }
            if name.is_some() {
                out.push("    }".to_owned());
            }
        }
        out.push("  }".to_owned());
        for side in SIDES {
            for (id, state) in self.ids[side].values().zip(&self.drawn[side]) {
                out.push(format!("  class {id} {}", state.class));
            }
        }
        (lines_text(&out), count)
    }

    fn id(&self, side: usize, state: &str) -> &str {
        self.ids[side]
            .get(&state.to_owned())
            .map_or("", String::as_str)
    }

    /// The region moves that make `label` from `at`, beside its state in the other region.
    fn takes(&self, at: &Parts, label: &str) -> Vec<(usize, String)> {
        SIDES
            .into_iter()
            .flat_map(|side| {
                self.moves[side]
                    .iter()
                    .filter(move |(_, m)| {
                        m.from == at[side] && m.label == label && m.beside.contains(&at[1 - side])
                    })
                    .map(move |(id, _)| (side, id.clone()))
            })
            .collect()
    }
}

impl Chart<'_> {
    /// Upstream's `regionsChart`, or `None` when some state does not read "context: progress".
    pub(super) fn regions(&self) -> Result<Option<RegionsChart>, ChartError> {
        let mut nodes: Vec<usize> = (0..self.keys.names.len()).collect();
        nodes.sort_by(|a, b| self.name(*a).cmp(self.name(*b)));
        let Some(parts) = nodes
            .iter()
            .map(|&key| split(self.name(key)).map(|parts| (key, parts)))
            .collect::<Option<HashMap<usize, Parts>>>()
        else {
            return Ok(None);
        };
        let mut steps: Vec<FlatStep> = Vec::new();
        let mut words: Vec<BTreeSet<String>> = Vec::new();
        for edge in self.edges.values() {
            if edge.labels.is_empty() {
                return Ok(Some(RegionsChart {
                    proof: None,
                    drawn: Err(NO_LABEL.to_owned()),
                }));
            }
            for label in edge.labels.iter() {
                steps.push(FlatStep {
                    from: parts[&edge.from].clone(),
                    to: parts[&edge.to].clone(),
                    label: norm(label),
                });
                words.push(
                    edge.guards
                        .as_ref()
                        .and_then(|guards| guards.get(label))
                        .cloned()
                        .unwrap_or_else(|| BTreeSet::from([String::new()])),
                );
            }
        }
        let starts: Vec<Parts> = self.initial.iter().map(|key| parts[key].clone()).collect();
        let flat = Machine {
            states: nodes.iter().map(|key| bar(&parts[key])).collect(),
            initial: starts.iter().map(bar).collect(),
            transitions: steps
                .iter()
                .map(|step| Transition {
                    from: bar(&step.from),
                    to: bar(&step.to),
                    label: step.label.clone(),
                    guard: None,
                })
                .collect(),
        };
        let proof = Some(proof(&flat));
        let refuse = |why: String| {
            Ok(Some(RegionsChart {
                proof: proof.clone(),
                drawn: Err(why),
            }))
        };

        let mut beside: [OrderedMap<String, OrderedSet<String>>; 2] = Default::default();
        for key in &nodes {
            let state = &parts[key];
            for side in SIDES {
                beside[side]
                    .entry(state[side].clone(), OrderedSet::default)
                    .insert(state[1 - side].clone());
            }
        }
        let size = beside[0].len() + beside[1].len();
        if size >= nodes.len() {
            return refuse(format!(
                "{size} region states are not fewer than the {} flat states",
                nodes.len()
            ));
        }

        let rank = |status: Status| RANK.iter().position(|&s| s == status).unwrap_or_default();
        let mut region_status: [OrderedMap<String, Status>; 2] = Default::default();
        for key in &nodes {
            for side in SIDES {
                let status = self.status(*key);
                let best = region_status[side].entry(parts[key][side].clone(), || status);
                if rank(status) < rank(*best) {
                    *best = status;
                }
            }
        }
        for key in &nodes {
            let shown = SIDES
                .into_iter()
                .filter_map(|side| region_status[side].get(&parts[key][side]))
                .map(|&status| rank(status))
                .max()
                .map_or(Status::Works, |at| RANK[at]);
            if shown != self.status(*key) {
                return refuse(format!(
                    "\"{}\" is \"{}\", and its two region states would show \"{}\"",
                    self.name(*key),
                    self.legend_words(self.status(*key)),
                    self.legend_words(shown)
                ));
            }
        }

        let mut moves: [OrderedMap<String, Move>; 2] = Default::default();
        for (step, words) in steps.iter().zip(&words) {
            for side in SIDES {
                let id = move_id(&step.from[side], &step.label, &step.to[side]);
                let m = moves[side].entry(id, || Move {
                    from: step.from[side].clone(),
                    label: step.label.clone(),
                    to: step.to[side].clone(),
                    beside: OrderedSet::default(),
                    words: BTreeSet::new(),
                });
                m.beside.insert(step.from[1 - side].clone());
                m.words.extend(words.iter().map(|w| norm(w)));
            }
        }
        for side_moves in &mut moves {
            let leaving: HashSet<(String, String)> = side_moves
                .values()
                .filter(|m| m.to != m.from)
                .map(|m| (m.from.clone(), m.label.clone()))
                .collect();
            side_moves.retain(|_, m| {
                m.from != m.to || leaving.contains(&(m.from.clone(), m.label.clone()))
            });
        }
        let ids: [OrderedMap<String, String>; 2] = SIDES.map(|side| {
            let mut states: Vec<&String> = beside[side].keys().collect();
            states.sort();
            states
                .into_iter()
                .enumerate()
                .map(|(index, state)| (state.clone(), format!("r{side}s{index}")))
                .fold(OrderedMap::default(), |mut ids, (state, id)| {
                    ids.insert(state, id);
                    ids
                })
        });
        let mut region_group: [OrderedMap<String, String>; 2] = Default::default();
        for key in &nodes {
            let group = self.group_of(*key);
            for side in SIDES {
                let mixed = region_group[side]
                    .get(&parts[key][side])
                    .is_some_and(|known| known != group);
                region_group[side].insert(
                    parts[key][side].clone(),
                    if mixed {
                        String::new()
                    } else {
                        group.to_owned()
                    },
                );
            }
        }
        let drawn: [Vec<DrawnState>; 2] = SIDES.map(|side| {
            ids[side]
                .keys()
                .map(|state| {
                    self.drawn_state(
                        region_group[side].get(state).map_or("", String::as_str),
                        region_status[side]
                            .get(state)
                            .copied()
                            .unwrap_or(Status::Works),
                        false,
                    )
                })
                .collect()
        });
        let regions = Regions {
            chart: self,
            nodes: nodes.len(),
            beside,
            moves,
            ids,
            drawn,
            starts,
        };

        let all = regions.draw(None);
        let differences = compare(&parse(&all.0)?.regions, &[(MAIN.to_owned(), flat)]);
        if let Some((kind, examples)) = differences.first() {
            let count: usize = differences.iter().map(|(_, found)| found.len()).sum();
            return refuse(format!(
                "the regions do not combine into the flat chart, with {count} difference(s), such as {kind}: {}",
                examples[0]
            ));
        }

        let (scoped, made) = self.scope(&regions, &steps, &parts, &nodes)?;
        let sound = made
            .states
            .iter()
            .all(|state| nodes.iter().any(|key| bar(&parts[key]) == *state))
            && {
                let real: HashSet<String> = steps.iter().map(FlatStep::key).collect();
                made.transitions
                    .iter()
                    .all(|t| real.contains(&step_key(&t.from, &t.label, &t.to)))
            };
        let scoped = if sound { scoped } else { all.clone() };
        let status = RANK
            .into_iter()
            .map(|status| {
                let count = region_status
                    .iter()
                    .map(|statuses| statuses.values().filter(|&&s| s == status).count())
                    .sum();
                (status, count)
            })
            .collect();
        Ok(Some(RegionsChart {
            proof,
            drawn: Ok(RegionsDrawn {
                mermaid: scoped.0,
                all_mermaid: all.0,
                states: [regions.beside[0].len(), regions.beside[1].len()],
                transitions: scoped.1,
                all: all.1,
                flat_states: nodes.len(),
                flat_transitions: steps.len(),
                status,
                groups: self.group_counts(&regions.drawn.concat()),
            }),
        }))
    }

    /// The arrows to draw: the bug paths, then the cheapest way to each region state and to each
    /// label no path has, and then, until the drawn arrows make only real steps, every arrow
    /// that a wrong step or a missing wanted step needs.
    fn scope(
        &self,
        regions: &Regions,
        steps: &[FlatStep],
        parts: &HashMap<usize, Parts>,
        nodes: &[usize],
    ) -> Result<((String, usize), Machine), ChartError> {
        let by_bar: HashMap<String, Parts> = nodes
            .iter()
            .map(|key| (bar(&parts[key]), parts[key].clone()))
            .collect();
        let real: HashSet<String> = steps.iter().map(FlatStep::key).collect();
        let mut next: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, step) in steps.iter().enumerate() {
            next.entry(bar(&step.from)).or_default().push(index);
        }
        let mut keep: [HashSet<String>; 2] = Default::default();
        let mut wanted: Vec<FlatStep> = Vec::new();
        for class in &self.classes {
            for (id, step) in class.steps() {
                if let Some(edge) = self.edges.get(&id) {
                    for wanted_step in steps_of(edge, &norm(&step.label), parts) {
                        want(&mut wanted, &mut keep, wanted_step);
                    }
                }
            }
            let turn = class.divergence.as_ref();
            if let Some(edge) = turn.and_then(|turn| self.edges.get(&turn.edge)) {
                for wanted_step in steps_of(edge, "", parts) {
                    want(&mut wanted, &mut keep, wanted_step);
                }
            }
        }
        let price = |step: &FlatStep, keep: &[HashSet<String>; 2]| {
            let unkept = step
                .moves()
                .iter()
                .filter(|(side, id)| !keep[*side].contains(id))
                .count();
            (unkept * regions.nodes + 1) as u64
        };
        let mut targets: Vec<Target> = Vec::new();
        for side in SIDES {
            let mut states: Vec<&String> = regions.beside[side].keys().collect();
            states.sort();
            targets.extend(
                states
                    .into_iter()
                    .map(|state| Target::At(side, state.clone())),
            );
        }
        let mut labels: Vec<&String> = steps
            .iter()
            .map(|step| &step.label)
            .collect::<OrderedSet<_>>()
            .iter()
            .copied()
            .filter(|label| !wanted.iter().any(|w| w.label == **label))
            .collect();
        labels.sort();
        targets.extend(labels.into_iter().map(|label| Target::Ends(label.clone())));
        let starts: Vec<String> = regions.starts.iter().map(bar).collect();
        while !targets.is_empty() {
            let (cost, parent) = cheapest(&starts, steps, &next, |step| price(step, &keep));
            let mut best: Option<(u64, String, Option<usize>, usize)> = None;
            for (n, target) in targets.iter().enumerate() {
                let ways: Vec<(String, u64, Option<usize>)> = match target {
                    Target::At(side, state) => cost
                        .iter()
                        .filter(|(at, _)| by_bar[*at][*side] == *state)
                        .map(|(at, &c)| (at.clone(), c, None))
                        .collect(),
                    Target::Ends(label) => steps
                        .iter()
                        .enumerate()
                        .filter(|(_, step)| step.label == *label)
                        .filter_map(|(index, step)| {
                            let from = bar(&step.from);
                            let c = *cost.get(&from)?;
                            Some((from, c + price(step, &keep), Some(index)))
                        })
                        .collect(),
                };
                for (at, c, last) in ways {
                    if best.as_ref().is_none_or(|(least, ..)| c < *least) {
                        best = Some((c, at, last, n));
                    }
                }
            }
            let Some((_, at, last, n)) = best else {
                break;
            };
            targets.remove(n);
            let mut path: Vec<usize> = last.into_iter().collect();
            let mut step = parent.get(&at).copied();
            while let Some(index) = step {
                path.push(index);
                step = parent.get(&bar(&steps[index].from)).copied();
            }
            for index in path {
                want(&mut wanted, &mut keep, steps[index].clone());
            }
        }
        loop {
            let scoped = regions.draw(Some(&keep));
            let made = product_of(&parse(&scoped.0)?.regions);
            let made_keys: HashSet<String> = made
                .transitions
                .iter()
                .map(|t| step_key(&t.from, &t.label, &t.to))
                .collect();
            let redo: Vec<(Parts, String)> = made
                .transitions
                .iter()
                .filter(|t| {
                    by_bar.contains_key(&t.from)
                        && !real.contains(&step_key(&t.from, &t.label, &t.to))
                })
                .map(|t| (by_bar[&t.from].clone(), t.label.clone()))
                .chain(
                    wanted
                        .iter()
                        .filter(|w| {
                            made.states.contains(&bar(&w.from)) && !made_keys.contains(&w.key())
                        })
                        .map(|w| (w.from.clone(), w.label.clone())),
                )
                .collect();
            let before = keep[0].len() + keep[1].len();
            for (at, label) in redo {
                for (side, id) in regions.takes(&at, &label) {
                    keep[side].insert(id);
                }
            }
            if keep[0].len() + keep[1].len() == before {
                return Ok((scoped, made));
            }
        }
    }
}

fn want(wanted: &mut Vec<FlatStep>, keep: &mut [HashSet<String>; 2], step: FlatStep) {
    for (side, id) in step.moves() {
        keep[side].insert(id);
    }
    wanted.push(step);
}

/// The flat steps an edge makes: only the one with `said`, when it has that label.
fn steps_of(edge: &Edge, said: &str, parts: &HashMap<usize, Parts>) -> Vec<FlatStep> {
    let labels: Vec<String> = edge.labels.iter().map(|label| norm(label)).collect();
    let chosen: Vec<String> = if labels.iter().any(|label| label == said) {
        vec![said.to_owned()]
    } else {
        labels
    };
    chosen
        .into_iter()
        .map(|label| FlatStep {
            from: parts[&edge.from].clone(),
            to: parts[&edge.to].clone(),
            label,
        })
        .collect()
}

/// The cheapest way from the start states to each flat state, as upstream's search finds it: it
/// settles the first open state of least cost, in the order the states opened.
fn cheapest(
    starts: &[String],
    steps: &[FlatStep],
    next: &HashMap<String, Vec<usize>>,
    price: impl Fn(&FlatStep) -> u64,
) -> (OrderedMap<String, u64>, HashMap<String, usize>) {
    let mut cost: OrderedMap<String, u64> = OrderedMap::default();
    for start in starts {
        cost.entry(start.clone(), || 0);
    }
    let mut parent: HashMap<String, usize> = HashMap::new();
    let mut open: OrderedSet<String> = cost.keys().cloned().collect();
    while let Some(here) = open
        .iter()
        .min_by_key(|state| cost.get(*state).copied().unwrap_or(u64::MAX))
        .cloned()
    {
        open.remove(&here);
        let base = cost.get(&here).copied().unwrap_or_default();
        for &index in next.get(&here).into_iter().flatten() {
            let step = &steps[index];
            let to = bar(&step.to);
            let through = base + price(step);
            if cost.get(&to).is_none_or(|&known| through < known) {
                cost.insert(to.clone(), through);
                parent.insert(to.clone(), index);
                open.insert(to);
            }
        }
    }
    (cost, parent)
}

fn proof(flat: &Machine) -> String {
    let ids: HashMap<&String, String> = flat
        .states
        .iter()
        .enumerate()
        .map(|(index, state)| (state, format!("p{index}")))
        .collect();
    let mut out = front_matter(None);
    out.push("stateDiagram-v2".to_owned());
    out.extend(
        flat.states
            .iter()
            .map(|state| format!("  state \"{}\" as {}", escape_quotes(state), ids[state])),
    );
    out.extend(
        flat.initial
            .iter()
            .map(|state| format!("  [*] --> {}", ids[state])),
    );
    out.extend(
        flat.transitions
            .iter()
            .map(|t| format!("  {} --> {} : {}", ids[&t.from], ids[&t.to], t.label)),
    );
    lines_text(&out)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("Offline: upload queued" => Some(["Offline".to_owned(), "upload queued".to_owned()]); "context and progress")]
    #[test_case(": nothing" => None; "no context")]
    #[test_case("Offline: " => None; "no progress")]
    #[test_case("Done;  now: a  b" => Some(["Done, now".to_owned(), "a b".to_owned()]); "semicolons and spaces")]
    fn splits_a_state(key: &str) -> Option<Parts> {
        split(key)
    }
}
