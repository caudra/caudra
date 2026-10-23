//! Writing a spec from the chart the reader agreed to, and checking that the spec draws it back:
//! upstream's `chart-to-tla.mjs`, `compare-charts.mjs`, and the comparison `from-chart` prints.
//! Where upstream writes `NAME.map.mjs`, this writes the Rhai mapping `NAME.map.rhai`.

use std::collections::{HashMap, HashSet};
use std::iter;
use std::path::Path;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use super::mapping::Mapping;
use super::mermaid::{Guard, Machine, MermaidError, compare, parse};
use super::text::{FULL_LABELS, LEGEND_ID};

/// The command the generated files name as the one that writes and keeps them.
pub const COMMAND: &str = "pstack.statechart from-chart";
const RESERVED: [&str; 6] = ["vars", "Init", "Next", "Spec", "TypeOK", "Settled"];
const TYPE_OK: &str = "TypeOK";
const UNNAMED: &str = "Step";
const VARIABLE_PREFIX: &str = "r";
const ACTION_PREFIX: &str = "S";
const SUFFIX_START: usize = 2;
const SPEC_NOTES: [&str; 2] = [
    "\\* Each region is one variable and each transition label is one action.",
    "\\* Once you add guards and variables from the real code, this file is the source.",
];
const READER_ACTIONS_NOTE: &str = "// The actions the reader takes. facts.settled needs them. Add cause() and detail() as the spec grows.";
const READER_STATES_NOTE: &str = "Reader states are the chart's own states.";
const STATE_JOIN: &str = " | ";
const PROBE: &str = "a region variable";

const SAME: &str = "same";
const SECOND_MODEL: &str =
    "it has a second model; draw ACTUAL without --fixed-dot or --against-dot";
const BOXES: &str = "it has boxes; draw ACTUAL with --flat";
const SHORT_LABELS: &str = "edge labels write shared words once; draw ACTUAL with --full-labels";
const NUMBERED: &str = "an edge label is a numbered path step; draw ACTUAL with --highlight 0";

static NOT_ALPHANUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]+").expect("valid regex"));
static MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]*$").expect("valid regex"));
static WORD_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^A-Za-z0-9]+)([A-Za-z0-9])").expect("valid regex"));
static SECOND_MODEL_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*%% second model").expect("valid regex"));
static BOX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*state\s+"[^"]*"\s+as\s+([A-Za-z0-9_]+)\s*\{"#).expect("valid regex")
});
static NUMBERED_STEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+(?:, [0-9]+)*\. ").expect("valid regex"));

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FromChartError {
    #[error(transparent)]
    Chart(#[from] MermaidError),
    #[error(
        "the module name {0} is not a TLA+ module name: it starts with a letter and has only letters, digits, and _"
    )]
    ModuleName(String),
    #[error("cannot make a TLA+ module name from {0}; add --module NAME")]
    NoModuleName(String),
    #[error("region {0} has no states")]
    NoStates(String),
    #[error("region {0} has no [*] --> start")]
    NoStart(String),
    #[error("never line names \"{0}\", which is not a state in the chart")]
    UnknownState(String),
    #[error("never line names \"{state}\", which is a state in regions {regions}; rename one")]
    AmbiguousState { state: String, regions: String },
    #[error("never name {0} is already used in the spec")]
    NameTaken(String),
    #[error("never {0} names two states of one region; one region is in one state at a time")]
    OneRegionTwice(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CompareError {
    #[error("{file}: {source}")]
    Unreadable { file: String, source: MermaidError },
    #[error("{file}: {why}")]
    NotForComparison { file: String, why: &'static str },
}

/// What `chart-to-tla.mjs` writes: `NAME.tla`, `NAME.cfg`, and the mapping `NAME.map.rhai`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub tla: String,
    pub cfg: String,
    pub mapping: String,
}

/// One chart and the file it came from, which messages name.
#[derive(Debug, Clone, Copy)]
pub struct ChartFile<'a> {
    pub name: &'a str,
    pub text: &'a str,
}

/// What differs between two charts, by kind, each sorted: what `compare-charts.mjs` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub differences: Vec<(&'static str, Vec<String>)>,
}

/// Whether the spec draws the agreed chart back, and the lines `from-chart` prints to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawnBack {
    pub same: bool,
    pub report: String,
}

struct Region<'a> {
    name: &'a str,
    var: String,
    machine: &'a Machine,
}

#[derive(PartialEq, Eq)]
struct Move<'a> {
    from: &'a str,
    to: &'a str,
    when: Option<String>,
}

struct Action<'a> {
    label: &'a str,
    moves: Vec<(usize, Vec<Move<'a>>)>,
}

struct Names(HashSet<String>);

impl Names {
    /// Upstream's `ident`: the words in CamelCase, made unique among every name in the spec.
    fn ident(&mut self, words: &str, lower: bool) -> String {
        let decomposed: String = words.nfkd().collect();
        let mut id: String = NOT_ALPHANUMERIC
            .split(&decomposed)
            .filter(|part| !part.is_empty())
            .enumerate()
            .map(|(index, part)| {
                let (first, rest) = part.split_at(1);
                let first = if lower && index == 0 {
                    first.to_ascii_lowercase()
                } else {
                    first.to_ascii_uppercase()
                };
                first + rest
            })
            .collect();
        if id.is_empty() {
            id = UNNAMED.to_owned();
        }
        if id.starts_with(|c: char| c.is_ascii_digit()) {
            id = format!(
                "{}{id}",
                if lower {
                    VARIABLE_PREFIX
                } else {
                    ACTION_PREFIX
                }
            );
        }
        let mut unique = id.clone();
        for n in SUFFIX_START.. {
            if !self.0.contains(&unique) {
                break;
            }
            unique = format!("{id}{n}");
        }
        self.0.insert(unique.clone());
        unique
    }
}

impl Comparison {
    pub fn is_same(&self) -> bool {
        self.differences.is_empty()
    }

    /// Each difference under its kind, then "N difference(s)", or "same".
    pub fn report(&self) -> String {
        let mut out = String::new();
        let mut count = 0;
        for (title, items) in &self.differences {
            out.push_str(&format!("{title}:\n"));
            for item in items {
                out.push_str(&format!("  {item}\n"));
            }
            count += items.len();
        }
        if count == 0 {
            out.push_str(SAME);
        } else {
            out.push_str(&format!("{count} difference(s)"));
        }
        out.push('\n');
        out
    }
}

/// The module name `from-chart` gives a chart without `--module`: its file name in CamelCase.
pub fn module_name(chart: &Path) -> Result<String, FromChartError> {
    let stem = chart
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_default();
    let name = WORD_START.replace_all(&stem, |found: &Captures| found[1].to_ascii_uppercase());
    if MODULE.is_match(&name) {
        Ok(name.into_owned())
    } else {
        Err(FromChartError::NoModuleName(chart.display().to_string()))
    }
}

/// Upstream's `chart-to-tla.mjs`: one variable per region, one action per transition label,
/// and one invariant per `%% never` line. `chart_file` names the chart in the files' comments.
pub fn chart_to_spec(
    chart_file: &str,
    chart: &str,
    module: &str,
) -> Result<Generated, FromChartError> {
    if !MODULE.is_match(module) {
        return Err(FromChartError::ModuleName(module.to_owned()));
    }
    let source = Path::new(chart_file)
        .file_name()
        .map_or_else(|| chart_file.into(), |name| name.to_string_lossy());
    let parsed = parse(chart)?;
    let mut names = Names(RESERVED.iter().map(|&name| name.to_owned()).collect());
    let regions = parsed
        .regions
        .iter()
        .map(|(name, machine)| {
            if machine.states.is_empty() {
                return Err(FromChartError::NoStates(name.clone()));
            }
            if machine.initial.is_empty() {
                return Err(FromChartError::NoStart(name.clone()));
            }
            Ok(Region {
                name,
                var: names.ident(name, true),
                machine,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let actions = actions(&regions);
    let action_names: Vec<String> = actions
        .iter()
        .map(|action| {
            let only = action.moves.first().and_then(|(_, moves)| moves.first());
            match only {
                Some(only) if action.label.is_empty() => {
                    names.ident(&format!("{} to {}", only.from, only.to), false)
                }
                _ => names.ident(action.label, false),
            }
        })
        .collect();

    let vars: Vec<&str> = regions.iter().map(|region| region.var.as_str()).collect();
    let mut out = vec![
        format!("---- MODULE {module} ----"),
        format!("\\* Generated by {COMMAND} from {source}."),
    ];
    out.extend(SPEC_NOTES.map(str::to_owned));
    out.push(format!(
        "\\* {COMMAND} {source} keeps it, checks it, and compares it with the chart."
    ));
    out.push(String::new());
    out.push(format!("VARIABLES {}", vars.join(", ")));
    out.push(format!("vars == <<{}>>", vars.join(", ")));
    out.push(String::new());
    for region in &regions {
        let mut states: Vec<&String> = region.machine.states.iter().collect();
        states.sort();
        out.push(format!("\\* Region {}", region.name));
        out.push(format!("{}States == {}", region.var, set(states)));
    }
    out.push(String::new());
    out.push("TypeOK ==".to_owned());
    out.extend(conjuncts(
        regions
            .iter()
            .map(|region| format!("{0} \\in {0}States", region.var)),
    ));
    out.push(String::new());
    out.push("Init ==".to_owned());
    out.extend(conjuncts(regions.iter().map(|region| {
        let initial = &region.machine.initial;
        match initial.iter().next() {
            Some(only) if initial.len() == 1 => format!("{} = {}", region.var, string(only)),
            _ => format!("{} \\in {}", region.var, set(initial.iter())),
        }
    })));
    for (action, name) in actions.iter().zip(&action_names) {
        out.push(String::new());
        if !action.label.is_empty() {
            out.push(format!("\\* {}", action.label));
        }
        out.push(format!("{name} =="));
        let shared = action.moves.len() > 1;
        if shared {
            let can: Vec<String> = action
                .moves
                .iter()
                .map(|(index, moves)| whether_takes(&regions[*index].var, moves).0)
                .collect();
            out.push(format!("    /\\ ({})", can.join(" \\/ ")));
        }
        for (index, moves) in &action.moves {
            let var = &regions[*index].var;
            let mut disjuncts: Vec<String> = moves
                .iter()
                .map(|m| format!("({} /\\ {var}' = {})", enabled(var, m), string(m.to)))
                .collect();
            if shared {
                let cannot = whether_takes(var, moves).1;
                disjuncts.push(format!("({cannot} /\\ UNCHANGED {var})"));
            }
            for (n, disjunct) in disjuncts.iter().enumerate() {
                let lead = if n == 0 { "    /\\ \\/" } else { "       \\/" };
                out.push(format!("{lead} {disjunct}"));
            }
        }
        let still: Vec<&str> = regions
            .iter()
            .enumerate()
            .filter(|(index, _)| !action.moves.iter().any(|(moving, _)| moving == index))
            .map(|(_, region)| region.var.as_str())
            .collect();
        if !still.is_empty() {
            out.push(format!("    /\\ UNCHANGED <<{}>>", still.join(", ")));
        }
    }
    out.push(String::new());
    out.push(if action_names.is_empty() {
        "Next == UNCHANGED vars".to_owned()
    } else {
        format!("Next == {}", action_names.join(" \\/ "))
    });
    out.push(String::new());
    out.push("Spec == Init /\\ [][Next]_vars".to_owned());

    let mut rules = vec![TYPE_OK.to_owned()];
    for never in &parsed.nevers {
        if !names.0.insert(never.name.clone()) {
            return Err(FromChartError::NameTaken(never.name.clone()));
        }
        let parts = never
            .states
            .iter()
            .map(|state| resolve(state, &regions))
            .collect::<Result<Vec<_>, _>>()?;
        let used: HashSet<usize> = parts.iter().map(|(index, _)| *index).collect();
        if used.len() != parts.len() {
            return Err(FromChartError::OneRegionTwice(never.name.clone()));
        }
        let condition: Vec<String> = parts
            .iter()
            .map(|(index, inside)| {
                let var = &regions[*index].var;
                match inside.as_slice() {
                    [only] => format!("{var} = {}", string(only)),
                    _ => format!("{var} \\in {}", set(inside.iter())),
                }
            })
            .collect();
        out.push(String::new());
        if !never.sentence.is_empty() {
            out.push(format!("\\* {}", never.sentence));
        }
        out.push(format!("{} == ~({})", never.name, condition.join(" /\\ ")));
        rules.push(never.name.clone());
    }
    out.extend(["", "====", ""].map(str::to_owned));

    let cfg: String = iter::once("SPECIFICATION Spec".to_owned())
        .chain(rules.iter().map(|rule| format!("INVARIANT {rule}")))
        .map(|line| line + "\n")
        .collect();
    let labels: Vec<(&str, &str)> = action_names
        .iter()
        .map(String::as_str)
        .zip(actions.iter().map(|action| action.label))
        .collect();
    Ok(Generated {
        tla: out.join("\n"),
        cfg,
        mapping: mapping(&source, &vars, &labels),
    })
}

/// Upstream's `compare-charts.mjs`: refuses an ACTUAL chart that is not drawn for comparison,
/// then compares the machines the two charts draw.
pub fn compare_charts(expected: ChartFile, actual: ChartFile) -> Result<Comparison, CompareError> {
    let refuse = |why| CompareError::NotForComparison {
        file: actual.name.to_owned(),
        why,
    };
    let boxes: Vec<&str> = BOX
        .captures_iter(actual.text)
        .filter_map(|found| found.get(1).map(|id| id.as_str()))
        .collect();
    if SECOND_MODEL_LINE.is_match(actual.text) {
        return Err(refuse(SECOND_MODEL));
    }
    if boxes.iter().any(|&id| id != LEGEND_ID) {
        return Err(refuse(BOXES));
    }
    if boxes.contains(&LEGEND_ID) && !actual.text.contains(FULL_LABELS) {
        return Err(refuse(SHORT_LABELS));
    }
    let read = |chart: ChartFile| {
        parse(chart.text).map_err(|source| CompareError::Unreadable {
            file: chart.name.to_owned(),
            source,
        })
    };
    let wanted = read(expected)?;
    let made = read(actual)?;
    let numbered = made
        .regions
        .iter()
        .flat_map(|(_, machine)| &machine.transitions)
        .any(|t| NUMBERED_STEP.is_match(&t.label));
    if numbered {
        return Err(refuse(NUMBERED));
    }
    Ok(Comparison {
        differences: compare(&wanted.regions, &made.regions),
    })
}

/// What `from-chart` prints after drawing the spec's overview `drawn` back: whether it is the
/// agreed `chart`, and each difference when it is not. The caller prints the blank line before it.
pub fn drawn_back(chart: ChartFile, drawn: ChartFile) -> Result<DrawnBack, CompareError> {
    let comparison = compare_charts(chart, drawn)?;
    let report = if comparison.is_same() {
        format!("The spec draws {} back unchanged.\n", chart.name)
    } else {
        let indented: String = comparison
            .report()
            .trim_end()
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect();
        format!("The spec does not draw {}:\n{indented}", chart.name)
    };
    Ok(DrawnBack {
        same: comparison.is_same(),
        report,
    })
}

/// Each transition label as one action, with each region's moves under it in region order; each
/// unlabeled transition is an action of its own.
fn actions<'a>(regions: &[Region<'a>]) -> Vec<Action<'a>> {
    let mut actions: Vec<Action> = Vec::new();
    let mut by_label: HashMap<&str, usize> = HashMap::new();
    for (index, region) in regions.iter().enumerate() {
        for t in &region.machine.transitions {
            let label = t.label.as_str();
            let at = match by_label.get(label) {
                Some(&at) if !label.is_empty() => at,
                _ => {
                    actions.push(Action {
                        label,
                        moves: Vec::new(),
                    });
                    if !label.is_empty() {
                        by_label.insert(label, actions.len() - 1);
                    }
                    actions.len() - 1
                }
            };
            let action = &mut actions[at];
            let position = match action.moves.iter().position(|(moving, _)| *moving == index) {
                Some(position) => position,
                None => {
                    action.moves.push((index, Vec::new()));
                    action.moves.len() - 1
                }
            };
            let moves = &mut action.moves[position].1;
            let next = Move {
                from: &t.from,
                to: &t.to,
                when: guard(t.guard.as_ref(), regions),
            };
            if !moves.contains(&next) {
                moves.push(next);
            }
        }
    }
    actions
}

fn guard(guard: Option<&Guard>, regions: &[Region]) -> Option<String> {
    let guard = guard?;
    let parts: Vec<String> = guard
        .states
        .iter()
        .map(|(name, states)| {
            let var = regions
                .iter()
                .find(|region| region.name == name)
                .map_or(name.as_str(), |region| region.var.as_str());
            match states.as_slice() {
                [only] => format!("{var} = {}", string(only)),
                _ => format!("{var} \\in {}", set(states.iter())),
            }
        })
        .collect();
    Some(if guard.not {
        format!("~({})", parts.join(" \\/ "))
    } else {
        parts.join(" /\\ ")
    })
}

fn enabled(var: &str, m: &Move) -> String {
    match &m.when {
        Some(when) => format!("{var} = {} /\\ {when}", string(m.from)),
        None => format!("{var} = {}", string(m.from)),
    }
}

/// When a region can take its part of a shared action, and when it cannot.
fn whether_takes(var: &str, moves: &[Move]) -> (String, String) {
    if moves.iter().all(|m| m.when.is_none()) {
        let mut from: Vec<&str> = Vec::new();
        for m in moves {
            if !from.contains(&m.from) {
                from.push(m.from);
            }
        }
        let from = set(from);
        return (
            format!("{var} \\in {from}"),
            format!("{var} \\notin {from}"),
        );
    }
    let any = moves
        .iter()
        .map(|m| format!("({})", enabled(var, m)))
        .collect::<Vec<_>>()
        .join(" \\/ ");
    (any.clone(), format!("~({any})"))
}

/// The region a never line's name is in, and the states it stands for there: the state itself,
/// or every state inside the composite of that name.
fn resolve<'a>(
    name: &str,
    regions: &[Region<'a>],
) -> Result<(usize, Vec<&'a String>), FromChartError> {
    let prefix = format!("{name}.");
    let hits: Vec<(usize, Vec<&String>)> = regions
        .iter()
        .enumerate()
        .filter_map(|(index, region)| {
            let states = &region.machine.states;
            let inside: Vec<&String> = match states.iter().find(|state| *state == name) {
                Some(state) => vec![state],
                None => states
                    .iter()
                    .filter(|state| state.starts_with(&prefix))
                    .collect(),
            };
            (!inside.is_empty()).then_some((index, inside))
        })
        .collect();
    match <[_; 1]>::try_from(hits) {
        Ok([hit]) => Ok(hit),
        Err(hits) if hits.is_empty() => Err(FromChartError::UnknownState(name.to_owned())),
        Err(hits) => Err(FromChartError::AmbiguousState {
            state: name.to_owned(),
            regions: hits
                .iter()
                .map(|(index, _)| regions[*index].name)
                .collect::<Vec<_>>()
                .join(" and "),
        }),
    }
}

/// The mapping upstream writes beside the spec, in Rhai: the chart's own states, joined by
/// " | " in region order, and each action's label.
fn mapping(source: &str, vars: &[&str], labels: &[(&str, &str)]) -> String {
    let state = vars
        .iter()
        .map(|var| format!("${{{}}}", property(var)))
        .collect::<Vec<_>>()
        .join(STATE_JOIN);
    let steps: String = labels
        .iter()
        .map(|(name, label)| format!("\t\t{name}: {},\n", rhai_string(label)))
        .collect();
    format!(
        "// Generated by {COMMAND} from {source}. {READER_STATES_NOTE}\n\
         \n\
         {READER_ACTIONS_NOTE}\n\
         fn reader_actions() {{\n\
         \t[]\n\
         }}\n\
         \n\
         fn state(v) {{\n\
         \t`{state}`\n\
         }}\n\
         \n\
         fn step(action) {{\n\
         \tlet labels = #{{\n\
         {steps}\t}};\n\
         \tlabels[action] ?? action\n\
         }}\n"
    )
}

/// `v.var`, or `v["var"]` where the mapping's engine reads `var` as a keyword.
fn property(var: &str) -> String {
    let dotted = format!("v.{var}");
    if Mapping::parse(PROBE, &format!("fn state(v) {{ {dotted} }}")).is_ok() {
        dotted
    } else {
        format!("v[{}]", rhai_string(var))
    }
}

fn rhai_string(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn conjuncts(lines: impl Iterator<Item = String>) -> impl Iterator<Item = String> {
    lines.map(|line| format!("    /\\ {line}"))
}

fn string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn set<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> String {
    let items: Vec<String> = items
        .into_iter()
        .map(|item| string(item.as_ref()))
        .collect();
    format!("{{{}}}", items.join(", "))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use test_case::test_case;

    use super::*;
    use crate::scripts::statechart::chart::{DrawOptions, draw};
    use crate::scripts::statechart::model::Model;
    use crate::scripts::statechart::testing::explore_text;
    use crate::scripts::statechart::text::rule_sentences;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statechart");
    const CHARTS: &str = "from_chart";
    const UPSTREAM_GENERATOR: &str = "chart-to-tla.mjs";
    const UPSTREAM_COMMAND: &str = "statechart.mjs from-chart";
    const MAPPING_NAME: &str = "Test.map.rhai";
    const DRAWN: &str = "chart.mmd";
    const AGREED: &str = "agreed.mmd";
    const ACTUAL: &str = "actual.mmd";
    const SPLIT: &str = "split.mmd";
    const SYNC: &str = "sync.mmd";
    const SYNC_MODULE: &str = "Sync";
    const SAME_REPORT: &str = "same\n";

    const DOOR: &str = r#"stateDiagram-v2
  state Door {
    [*] --> Locked
    Locked --> Unlocked : tap unlock
    Unlocked --> Locked : tap lock
    --
    [*] --> Closed
    Closed --> Open : open the door
    Open --> Closed : close the door
  }
  [*] --> Door"#;
    const FLAT_DOOR: &str = r#"stateDiagram-v2
  state "Locked | Closed" as L_C
  state "Locked | Open" as L_O
  state "Unlocked | Closed" as U_C
  state "Unlocked | Open" as U_O
  [*] --> L_C
  L_C --> U_C : tap unlock
  U_C --> L_C : tap lock
  L_O --> U_O : tap unlock
  L_C --> L_O : open the door
  L_O --> L_C : close the door
  U_C --> U_O : open the door
  U_O --> U_C : close the door"#;
    const LAMP: &str = r#"stateDiagram-v2
  state "Lamp" as lamp {
    state "Power" as power {
      [*] --> Off
      Off --> On : flip the switch
      On --> Off : flip the switch
    }
    --
    state "Bulb" as bulb {
      [*] --> Dark
      Dark --> Lit : warm up [when the<br/>power is on]
      %% when On
      Lit --> Dark : cool down [when the power is off]
      %% when not On
    }
  }
  %% never LitWhileOff: Off & Lit -- The bulb is never lit while the power is off.
"#;
    const FLAT_LAMP: &str = r#"stateDiagram-v2
  [*] --> OD
  state "Off | Dark" as OD
  state "On | Dark" as ND
  state "On | Lit" as NL
  state "Off | Lit" as OL
  OD --> ND : flip the switch
  ND --> OD : flip the switch
  ND --> NL : warm up
  NL --> OL : flip the switch
  OL --> NL : flip the switch
  OL --> OD : cool down"#;
    const SWITCH: &str = r#"stateDiagram-v2
  state "Lamp" as lamp {
    state "Power" as power {
      [*] --> Off
      Off --> On : flip the switch
      On --> Off : flip the switch
    }
    --
    state "Bulb" as bulb {
      [*] --> Dark
      Dark --> Lit : flip the switch [when the power is off]
      %% when Off
      Lit --> Dark : flip the switch
    }
  }
"#;
    const SWITCH_ACTION: &str = r#"\* flip the switch
FlipTheSwitch ==
    /\ (power \in {"Off", "On"} \/ (bulb = "Dark" /\ power = "Off") \/ (bulb = "Lit"))
    /\ \/ (power = "Off" /\ power' = "On")
       \/ (power = "On" /\ power' = "Off")
       \/ (power \notin {"Off", "On"} /\ UNCHANGED power)
    /\ \/ (bulb = "Dark" /\ power = "Off" /\ bulb' = "Lit")
       \/ (bulb = "Lit" /\ bulb' = "Dark")
       \/ (~((bulb = "Dark" /\ power = "Off") \/ (bulb = "Lit")) /\ UNCHANGED bulb)
"#;
    const NEXT: &str = "\nNext ==";
    const NEVER_NO_STATE: &str =
        "stateDiagram-v2\n  [*] --> A\n  A --> B : go\n  %% never Bad: C -- never C\n";
    const NEVERS_BY_STATE_AND_COMPOSITE: &str = r#"stateDiagram-v2
  state "Saving" as S
  state "Saving..." as S2
  state Busy {
    [*] --> Checking
    Checking --> Sending : the check passes
  }
  [*] --> S
  S --> S2 : you wait
  S2 --> Busy : you send
  Busy --> S : you cancel
  %% never NoSaving: Saving -- You never see Saving.
  %% never NoBusy: Busy -- You are never busy.
"#;
    const SPLIT_OUTSIDE: &str = "stateDiagram-v2\n  [*] --> Online\n  Online --> Offline : the network drops\n  --\n  [*] --> Synced\n  Synced --> Pending : you edit a note\n";
    const ONE_REGION: &str =
        "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Busy : start\n  Busy --> Idle : finish\n";
    const GUARD_ON_SERVER: (&str, &str) = (
        "TheServerConfirms ==\n",
        "TheServerConfirms ==\n    /\\ connection = \"Online\"\n",
    );
    const SYNC_NOT_DRAWN: &str = "The spec does not draw sync.mmd:
  states only in EXPECTED (no path from the start reaches them in the spec, or the spec renamed them):
    Offline | Synced
  transitions only in EXPECTED (a guard in the spec blocks them, or the spec renamed the step):
    Offline | Synced --> Offline | Waiting to sync : the network drops
    Offline | Synced --> Offline | Waiting to sync : you edit a note
    Offline | Synced --> Online | Synced : the network returns
    Offline | Waiting to sync --> Offline | Synced : the server confirms
  5 difference(s)
";

    fn fixture(name: &str) -> String {
        fs::read_to_string(PathBuf::from(FIXTURES).join(name)).expect("the fixture reads")
    }

    fn chart_fixture(name: &str) -> String {
        fixture(&format!("{CHARTS}/{name}"))
    }

    fn generate(chart: &str, module: &str) -> Generated {
        chart_to_spec(&format!("{module}.mmd"), chart, module).expect("the chart generates")
    }

    fn chart_file<'a>(name: &'a str, text: &'a str) -> ChartFile<'a> {
        ChartFile { name, text }
    }

    fn compared(expected: &str, actual: &str) -> Result<Comparison, CompareError> {
        compare_charts(chart_file(AGREED, expected), chart_file(ACTUAL, actual))
    }

    /// Explores the generated spec and draws its overview as `from-chart` does.
    fn draw_back(
        name: &str,
        chart: &str,
        module: &str,
        generated: &Generated,
    ) -> (Model, DrawnBack) {
        let model = explore_text(module, &generated.tla, &generated.cfg);
        let mapping = Mapping::parse(MAPPING_NAME, &generated.mapping).expect("the mapping parses");
        let options = DrawOptions {
            rules: rule_sentences(&generated.tla),
            comparing: true,
            ..DrawOptions::default()
        };
        let charts = draw(&model, &mapping, &options).expect("the charts draw");
        let back = drawn_back(chart_file(name, chart), chart_file(DRAWN, &charts.chart))
            .expect("the charts compare");
        (model, back)
    }

    #[test_case("sync.mmd", "Sync"; "sync")]
    #[test_case("upload.mmd", "Upload"; "upload")]
    fn writes_upstreams_spec_and_config_and_the_fixture_mapping(chart: &str, module: &str) {
        let generated = chart_to_spec(chart, &chart_fixture(chart), module).expect("generates");

        let upstream = fixture(&format!("{module}.tla"))
            .replace(UPSTREAM_COMMAND, COMMAND)
            .replace(UPSTREAM_GENERATOR, COMMAND);
        assert_eq!(generated.tla, upstream);
        assert_eq!(generated.cfg, fixture(&format!("{module}.cfg")));
        assert_eq!(generated.mapping, fixture(&format!("{module}.map.rhai")));
    }

    #[test_case("sync.mmd", "Sync", 4, 9; "sync")]
    #[test_case("upload.mmd", "Upload", 5, 8; "upload")]
    fn the_generated_spec_explores_to_upstreams_graph_and_draws_the_chart_back(
        chart: &str,
        module: &str,
        states: usize,
        steps: usize,
    ) {
        let text = chart_fixture(chart);
        let generated = generate(&text, module);

        let (model, back) = draw_back(chart, &text, module, &generated);

        assert_eq!((model.states.len(), model.steps.len()), (states, steps));
        assert_eq!(
            back,
            DrawnBack {
                same: true,
                report: format!("The spec draws {chart} back unchanged.\n"),
            }
        );
    }

    #[test_case(LAMP, "Lamp"; "regions that guard each other")]
    #[test_case(SWITCH, "Switch"; "a label with a guarded and an unguarded move in one region")]
    fn a_guarded_chart_draws_back_unchanged(chart: &str, module: &str) {
        let (_, back) = draw_back(AGREED, chart, module, &generate(chart, module));

        assert!(back.same, "{}", back.report);
    }

    #[test]
    fn a_guard_in_the_spec_shows_as_each_step_the_chart_has_and_the_spec_does_not() {
        let text = chart_fixture(SYNC);
        let mut generated = generate(&text, SYNC_MODULE);
        let (step, guarded) = GUARD_ON_SERVER;
        generated.tla = generated.tla.replace(step, guarded);

        let (_, back) = draw_back(SYNC, &text, SYNC_MODULE, &generated);

        assert_eq!(
            back,
            DrawnBack {
                same: false,
                report: SYNC_NOT_DRAWN.to_owned(),
            }
        );
    }

    #[test]
    fn a_never_line_names_a_state_exactly_or_every_state_inside_a_composite() {
        let generated = generate(NEVERS_BY_STATE_AND_COMPOSITE, "C");

        let lines: Vec<&str> = generated.tla.lines().collect();
        assert!(lines.contains(&r#"NoSaving == ~(main = "Saving")"#));
        assert!(lines.contains(&r#"NoBusy == ~(main \in {"Busy.Checking", "Busy.Sending"})"#));
        assert_eq!(
            generated.cfg,
            "SPECIFICATION Spec\nINVARIANT TypeOK\nINVARIANT NoSaving\nINVARIANT NoBusy\n"
        );
        assert!(generated.mapping.contains("fn reader_actions() {\n\t[]\n}"));
    }

    #[test]
    fn a_when_line_guards_a_move_on_the_other_regions_states() {
        let generated = generate(LAMP, "Lamp");

        assert!(
            generated
                .tla
                .contains(r#"(bulb = "Dark" /\ power = "On" /\ bulb' = "Lit")"#)
        );
        assert!(
            generated
                .tla
                .contains(r#"(bulb = "Lit" /\ ~(power = "On") /\ bulb' = "Dark")"#)
        );
        assert_eq!(
            compared(LAMP, FLAT_LAMP).map(|c| c.report()),
            Ok(SAME_REPORT.to_owned())
        );
    }

    #[test]
    fn a_label_with_a_guarded_and_an_unguarded_move_in_one_region_writes_both_moves() {
        let tla = generate(SWITCH, "Switch").tla;

        let start = tla.find(SWITCH_ACTION.lines().next().unwrap_or_default());
        let end = tla.find(NEXT);
        let action = start.zip(end).map(|(start, end)| &tla[start..end]);
        assert_eq!(action, Some(SWITCH_ACTION));
    }

    #[test]
    fn names_are_camel_case_unique_and_never_start_with_a_digit() {
        let generated = generate(
            "stateDiagram-v2\n  state Top {\n    state \"If\" as first {\n      [*] --> A\n      A --> B\n      B --> A : 3-D secure\n      A --> A : init\n    }\n    --\n    state \"Mode\" as mode {\n      [*] --> Quiet\n    }\n  }\n",
            "Names",
        );

        assert!(generated.tla.contains("VARIABLES if, mode\n"));
        assert!(
            generated
                .tla
                .contains("\nNext == AToB \\/ S3DSecure \\/ Init2\n")
        );
        assert!(generated.mapping.contains("\t`${v[\"if\"]} | ${v.mode}`\n"));
        assert!(generated.mapping.contains("\t\tAToB: \"\",\n"));
        Mapping::parse(MAPPING_NAME, &generated.mapping).expect("the mapping parses");
    }

    #[test]
    fn accented_words_keep_their_letters_as_upstreams_nfkd_does() {
        let generated = generate(
            "stateDiagram-v2\n  state \"Café\" as open\n  state \"Fermé\" as shut\n  [*] --> open\n  open --> shut : on ferme ﬁnalement\n",
            "Accents",
        );

        assert!(
            generated.tla.contains("OnFermeFinalement"),
            "{}",
            generated.tla
        );
    }

    #[test_case(NEVER_NO_STATE, "C" => FromChartError::UnknownState("C".to_owned()); "a never line that names no state")]
    #[test_case("stateDiagram-v2\n  [*] --> A\n  A --> B : go\n  %% never Both: A & B -- never both\n", "C" => FromChartError::OneRegionTwice("Both".to_owned()); "a never line with two states of one region")]
    #[test_case("stateDiagram-v2\n  [*] --> A\n  A --> B : go\n  %% never TypeOK: A -- never A\n", "C" => FromChartError::NameTaken("TypeOK".to_owned()); "a never name the spec has")]
    #[test_case("stateDiagram-v2\n  [*] --> A\n  A --> B : go\n  %% never Go: A -- never A\n", "C" => FromChartError::NameTaken("Go".to_owned()); "a never name an action has")]
    #[test_case("stateDiagram-v2\n  state Top {\n    state \"Upload\" as up {\n      [*] --> U\n      state \"Busy\" as b1 {\n        [*] --> X\n      }\n      U --> b1 : go\n    }\n    --\n    state \"Sync\" as sync {\n      [*] --> V\n      state \"Busy\" as b2 {\n        [*] --> Y\n      }\n      V --> b2 : go\n    }\n  }\n  %% never Busy2: Busy -- never busy\n", "C" => FromChartError::AmbiguousState { state: "Busy".to_owned(), regions: "Upload and Sync".to_owned() }; "a never line with a state in two regions")]
    #[test_case("stateDiagram-v2\n  A --> B : go\n", "C" => FromChartError::NoStart("Main".to_owned()); "a region without a start")]
    #[test_case(SPLIT_OUTSIDE, "Split" => FromChartError::Chart(MermaidError::TopLevelSplit); "dashes outside a composite")]
    #[test_case(ONE_REGION, "1Up" => FromChartError::ModuleName("1Up".to_owned()); "a module name that is not one")]
    fn refuses(chart: &str, module: &str) -> FromChartError {
        chart_to_spec(&format!("{module}.mmd"), chart, module).expect_err("refuses")
    }

    #[test_case("      %% when On\n", "      state Glow\n      %% when On\n" => matches FromChartError::Chart(MermaidError::StrayWhen(_)); "a when line after no transition")]
    #[test_case("warm up [when the<br/>power is on]", "warm up" => matches FromChartError::Chart(MermaidError::UnwordedGuard(_)); "a guard without words")]
    #[test_case("      %% when On\n", "      %% when Nope\n" => matches FromChartError::Chart(MermaidError::UnknownGuardState(state)) if state == "Nope"; "a guard on no state")]
    #[test_case("      %% when On\n", "      %% when Lit\n" => matches FromChartError::Chart(MermaidError::OwnRegionGuard(state)) if state == "Lit"; "a guard on its own region")]
    fn refuses_a_guard_it_cannot_read(from: &str, to: &str) -> FromChartError {
        chart_to_spec(AGREED, &LAMP.replace(from, to), "Lamp").expect_err("refuses")
    }

    #[test_case("notes-app.mmd" => Ok("NotesApp".to_owned()); "dashes")]
    #[test_case("charts/sync.mmd" => Ok("Sync".to_owned()); "a folder")]
    #[test_case("my_chart v2.mmd" => Ok("MyChartV2".to_owned()); "underscores and spaces")]
    #[test_case("3d.mmd" => Err(FromChartError::NoModuleName("3d.mmd".to_owned())); "a leading digit")]
    fn module_names(chart: &str) -> Result<String, FromChartError> {
        module_name(Path::new(chart))
    }

    #[test]
    fn compare_reads_parallel_regions_as_one_machine_so_a_guard_across_regions_shows() {
        let free = format!("{FLAT_DOOR}\n  U_O --> L_O : tap lock");

        assert_eq!(
            compared(DOOR, FLAT_DOOR).map(|c| c.report()),
            Ok("transitions only in EXPECTED (a guard in the spec blocks them, or the spec renamed the step):\n  Unlocked | Open --> Locked | Open : tap lock\n1 difference(s)\n".to_owned())
        );
        assert_eq!(
            compared(DOOR, &free).map(|c| c.report()),
            Ok(SAME_REPORT.to_owned())
        );
    }

    #[test]
    fn compare_reads_one_region_as_the_machine_its_steps_reach_without_steps_that_change_nothing() {
        let looping = format!("{ONE_REGION}  Idle --> Idle : refresh\n");
        let orphan = format!("{ONE_REGION}  Lost --> Idle : come back\n");

        assert_eq!(
            compared(&looping, ONE_REGION).map(|c| c.report()),
            Ok(SAME_REPORT.to_owned())
        );
        assert_eq!(
            compared(&orphan, ONE_REGION).map(|c| c.report()),
            Ok(
                "states the agreed chart's own steps never reach:\n  Lost\n1 difference(s)\n"
                    .to_owned()
            )
        );
    }

    #[test_case("  U_O --> L_O : 1. tap lock" => NUMBERED; "a numbered path step")]
    #[test_case("  %% second model (against): the spec" => SECOND_MODEL; "a second model")]
    #[test_case("  state \"Locked\" as g1 {\n    state \"Closed\" as L_C2\n  }" => BOXES; "boxes")]
    #[test_case("  state \"Legend\" as legend {\n    state \"Works\" as legendWorks\n  }" => SHORT_LABELS; "labels that share words")]
    fn compare_refuses_an_actual_chart_not_drawn_for_comparison(extra: &str) -> &'static str {
        match compared(DOOR, &format!("{FLAT_DOOR}\n{extra}")) {
            Err(CompareError::NotForComparison { file, why }) if file == ACTUAL => why,
            other => panic!("expected a refusal of {ACTUAL}, got {other:?}"),
        }
    }

    #[test_case(SPLIT, SPLIT; "both")]
    #[test_case(AGREED, SPLIT; "actual")]
    #[test_case(SPLIT, AGREED; "expected")]
    fn compare_names_the_chart_it_cannot_read(expected: &str, actual: &str) {
        let text = |name| if name == SPLIT { SPLIT_OUTSIDE } else { DOOR };

        let refused = compare_charts(
            chart_file(expected, text(expected)),
            chart_file(actual, text(actual)),
        );

        assert_eq!(
            refused,
            Err(CompareError::Unreadable {
                file: SPLIT.to_owned(),
                source: MermaidError::TopLevelSplit,
            })
        );
    }
}
