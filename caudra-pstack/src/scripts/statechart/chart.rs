//! Upstream's `tlc-to-chart.mjs`: it collapses the model onto the reader's states, groups the
//! counterexamples into bugs, and draws the Mermaid chart. With a second model, the fix or the
//! spec the code must follow, the chart also draws the steps only it takes, each bug's replay on
//! it, and its own bugs, and marks states fixed.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use rhai::Dynamic;
use serde_json::Value as Json;
use thiserror::Error;

use super::mapping::{self, Mapping, MappingError};
use super::mermaid::MermaidError;
use super::model::{Model, Trace, Vars};
use super::ordered::{OrderedMap, OrderedSet};
use super::second::{self, DEFAULT_AGAINST_LABEL, Replay, Role, SecondModel, Turn};
use super::story;
use super::text::{
    FULL_LABELS, GROUP_PALETTE, LEGEND_ID, PLAIN_FILL, STATE_WIDTH, STATUS_CLASSES, STEP_WIDTH,
    collapse_spaces, escape_quotes, group_class_def, group_color_problem, join_labels, wrap,
};

/// Joins the parts of a layered key, as upstream's `SEP`.
pub(super) const SEP: char = '\0';
const CONTEXT: &str = ": ";
const OR: &str = " or ";
pub(super) const BREAK: &str = "<br/>";
const GROUP_GAP: &str = "<br/>#160;<br/>";
const STATEMENT_END: char = ';';
const STATEMENT_END_STANDIN: &str = ",";
const STATUS_GROUP_NAMES: [&str; 5] = [
    "Works",
    "Works today",
    "Breaks a rule",
    "Still wrong",
    "Fixed",
];
const DEFAULT_WORKS: &str = "Works";
const DEFAULT_WRONG: &str = "Breaks a rule";
const FIX_WORKS: &str = "Works today";
const CANNOT_HAPPEN: &str = " cannot happen";
const PLAIN: &str = "plain";
const PATH: &str = "Path";
const BORDERS: [&str; 4] = ["", "Wrong", "Fixed", PATH];
const BUG_FILE: &str = "bug-";
const MERMAID_EXTENSION: &str = ".mmd";

/// Per label of one edge: how many model steps it has, and the guard words each has.
type LabelSteps = (OrderedMap<String, usize>, HashMap<String, BTreeSet<String>>);

#[derive(Debug, Error)]
pub enum ChartError {
    #[error(transparent)]
    Mapping(#[from] MappingError),
    #[error("the {0} view needs `fn detail(v, facts)` in the mapping")]
    NoDetail(&'static str),
    #[error("bug {highlight} does not exist: the model has {count} bugs")]
    NoSuchBug { highlight: usize, count: usize },
    #[error(
        "regions draw the whole customer chart: use the customer view, highlight no bug, and draw neither flat nor only the path"
    )]
    RegionsOptions,
    #[error(
        "group \"{0}\" has the name of a status; the model checker decides the status, so name the group for what the reader sees"
    )]
    StatusGroup(String),
    #[error("group_colors: \"{group}\" {color:?} {problem}")]
    GroupColor {
        group: String,
        color: String,
        problem: &'static str,
    },
    #[error(
        "the mapping has {0} groups, more than the {count} colors of the palette; give the others a color in group_colors",
        count = GROUP_PALETTE.len()
    )]
    Palette(usize),
    #[error("the chart it drew does not read back: {0}")]
    Reread(#[from] MermaidError),
}

/// Which label names a reader state: the mapping's `state`, its `detail`, or each detail inside
/// its state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Customer,
    Developer,
    Layered,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            Self::Customer => "customer",
            Self::Developer => "developer",
            Self::Layered => "layered",
        }
    }

    fn file_suffix(self) -> &'static str {
        match self {
            Self::Customer => "",
            Self::Developer => "-developer",
            Self::Layered => "-layered",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Status {
    Works,
    Wrong,
    Fixed,
}

impl Status {
    pub(super) const ALL: [Self; 3] = [Self::Works, Self::Wrong, Self::Fixed];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Works => "works",
            Self::Wrong => "wrong",
            Self::Fixed => "fixed",
        }
    }

    fn border(self) -> &'static str {
        match self {
            Self::Works => "",
            Self::Wrong => "Wrong",
            Self::Fixed => "Fixed",
        }
    }

    fn legend_id(self) -> &'static str {
        match self {
            Self::Works => "legendWorks",
            Self::Wrong => "legendWrong",
            Self::Fixed => "legendFixed",
        }
    }
}

/// One chart, as one run of upstream's `tlc-to-chart.mjs` draws it.
#[derive(Debug, Clone, Default)]
pub struct ChartOptions {
    pub view: View,
    /// The bug drawn as a numbered path, from 1; 0 draws none.
    pub highlight: usize,
    pub path_only: bool,
    pub full_labels: bool,
    pub flat: bool,
    /// Only the shortest path to each settled state, plus the bug paths.
    pub cover: bool,
    pub regions: bool,
    pub title: Option<String>,
}

/// What one `from-spec` or `from-chart` run draws.
#[derive(Debug, Clone, Default)]
pub struct DrawOptions {
    pub title: Option<String>,
    /// Each rule's sentence, from the spec's comments.
    pub rules: HashMap<String, String>,
    /// Drawing the agreed chart back to compare it: every label in full, no boxes, no regions.
    pub comparing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugChart {
    pub file: String,
    pub mermaid: String,
}

/// The files of one run: `chart.mmd`, `bug-N[-view].mmd`, `stories.md`, `summary.json`, and the
/// regions proof upstream keeps in `tlc/`.
#[derive(Debug, Clone)]
pub struct Charts {
    pub chart: String,
    pub bugs: Vec<BugChart>,
    pub stories: String,
    pub summary: Json,
    pub regions_proof: Option<String>,
    pub regions_all: Option<String>,
}

/// One chart and what upstream's `--story` and `--summary` write beside it.
#[derive(Debug, Clone)]
pub struct Drawing {
    pub mermaid: String,
    pub stories: String,
    pub summary: Json,
    pub regions_proof: Option<String>,
    pub regions_all: Option<String>,
}

/// Draws what one run writes: the overview, then each bug in each view the mapping has.
pub fn draw(model: &Model, mapping: &Mapping, options: &DrawOptions) -> Result<Charts, ChartError> {
    draw_with(model, None, mapping, options)
}

/// Draws what one run with a second model writes: the bugs only the second model has follow the
/// model's own, each with its charts.
pub fn draw_with(
    model: &Model,
    second: Option<&SecondModel>,
    mapping: &Mapping,
    options: &DrawOptions,
) -> Result<Charts, ChartError> {
    let charter = Charter::with_second(model, second, mapping, &options.rules)?;
    let cover = !options.comparing && !mapping.reader_actions.is_empty();
    let overview = charter.chart(&ChartOptions {
        full_labels: options.comparing,
        flat: options.comparing,
        cover,
        regions: !options.comparing,
        title: options.title.clone(),
        ..ChartOptions::default()
    })?;
    let views: &[View] = if mapping.has_detail() {
        &[View::Customer, View::Developer, View::Layered]
    } else {
        &[View::Customer]
    };
    let mut bugs = Vec::new();
    for highlight in 1..=charter.bugs.len() {
        for &view in views {
            let drawing = charter.chart(&ChartOptions {
                view,
                highlight,
                path_only: true,
                full_labels: options.comparing,
                flat: options.comparing,
                cover,
                regions: false,
                title: options
                    .title
                    .as_ref()
                    .map(|title| format!("{title}, bug {highlight}")),
            })?;
            bugs.push(BugChart {
                file: format!(
                    "{BUG_FILE}{highlight}{}{MERMAID_EXTENSION}",
                    view.file_suffix()
                ),
                mermaid: drawing.mermaid,
            });
        }
    }
    Ok(Charts {
        chart: overview.mermaid,
        bugs,
        stories: overview.stories,
        summary: overview.summary,
        regions_proof: overview.regions_proof,
        regions_all: overview.regions_all,
    })
}

#[derive(Debug, Clone)]
pub(super) struct StepWords {
    said: String,
    seconds: Option<f64>,
    guard: String,
}

impl StepWords {
    /// Upstream's `stepLabel`: the mapping's words, the action beside them outside the customer
    /// view, and the timer's wait before them.
    pub(super) fn label(&self, view: View, action: &str) -> String {
        let named = if view == View::Customer || self.said == action {
            self.said.clone()
        } else {
            format!("{} [{action}]", self.said)
        };
        match self.seconds {
            Some(seconds) if !named.is_empty() => format!("after {seconds} s, {named}"),
            _ => named,
        }
    }
}

/// A bug: the counterexamples that break one rule for one cause, or at one end state.
#[derive(Debug)]
pub(super) struct Bug {
    pub(super) trace: usize,
    pub(super) cause: Option<String>,
    pub(super) count: usize,
    /// Only the second model has this bug, and `trace` is one of its traces.
    pub(super) introduced: bool,
    /// How the bug fares on the second model, when there is one.
    pub(super) replay: Option<Replay>,
    steps: usize,
    id: String,
}

#[derive(Debug, Default)]
pub(super) struct Palette {
    pub(super) names: Vec<String>,
    pub(super) fills: HashMap<String, String>,
}

/// The mapping's words for each state and step of one model, read once.
pub(super) struct Side<'a> {
    pub(super) model: &'a Model,
    vars: Vec<Dynamic>,
    facts: Vec<Dynamic>,
    settled: Vec<bool>,
    customer: Vec<String>,
    detail: Vec<String>,
    groups: Vec<String>,
    pub(super) words: Vec<StepWords>,
    step_at: HashMap<(usize, usize, Arc<str>), usize>,
    /// Each state's steps, as indexes of the model's: its moves, then its loops, as upstream
    /// lists a graph's edges before its loops.
    pub(super) out: Vec<Vec<usize>>,
}

impl<'a> Side<'a> {
    fn new(model: &'a Model, mapping: &Mapping, vars: Vec<Dynamic>) -> Result<Self, ChartError> {
        let count = model.states.len();
        let (loops, moves): (Vec<usize>, Vec<usize>) = (0..model.steps.len())
            .partition(|&index| model.steps[index].from == model.steps[index].to);
        let mut enabled: Vec<OrderedSet<&str>> = vec![OrderedSet::default(); count];
        let mut out = vec![Vec::new(); count];
        for &index in moves.iter().chain(&loops) {
            let step = &model.steps[index];
            enabled[step.from].insert(step.action.as_deref().unwrap_or_default());
            out[step.from].push(index);
        }
        let settled: Vec<bool> = enabled
            .iter()
            .map(|actions| mapping.settled(actions.iter().copied()))
            .collect();
        let facts: Vec<Dynamic> = enabled
            .iter()
            .zip(&settled)
            .map(|(actions, &settled)| mapping::facts(actions.iter().copied(), settled))
            .collect();
        let mut customer = Vec::with_capacity(count);
        let mut detail = Vec::new();
        let mut groups = Vec::new();
        for (vars, facts) in vars.iter().zip(&facts) {
            customer.push(mapping.state(vars, facts)?);
            if let Some(words) = mapping.detail(vars, facts)? {
                detail.push(words);
            }
            if mapping.has_group() {
                let group = mapping.group(vars, facts)?.unwrap_or_default();
                groups.push(collapse_spaces(&group));
            }
        }
        let mut words = Vec::with_capacity(model.steps.len());
        let mut step_at = HashMap::with_capacity(model.steps.len());
        for (index, step) in model.steps.iter().enumerate() {
            let action = step.action.clone().unwrap_or_default();
            words.push(step_words(
                mapping,
                &action,
                &vars[step.from],
                &vars[step.to],
            )?);
            step_at.insert((step.from, step.to, action), index);
        }
        Ok(Self {
            model,
            vars,
            facts,
            settled,
            customer,
            detail,
            groups,
            words,
            step_at,
            out,
        })
    }

    pub(super) fn step_words(
        &self,
        mapping: &Mapping,
        from: usize,
        to: usize,
        action: &Arc<str>,
    ) -> Result<StepWords, ChartError> {
        match self.step_at.get(&(from, to, action.clone())) {
            Some(&index) => Ok(self.words[index].clone()),
            None => Ok(step_words(
                mapping,
                action,
                &self.vars[from],
                &self.vars[to],
            )?),
        }
    }

    fn name(&self, state: usize, view: View) -> String {
        match view {
            View::Customer => self.customer[state].clone(),
            View::Developer => self.detail[state].clone(),
            View::Layered => format!("{}{SEP}{}", self.customer[state], self.detail[state]),
        }
    }
}

/// Which states of one model break a rule: the end of each counterexample, and each state that
/// no good start reaches through good states only.
struct Health {
    bad: Vec<bool>,
    clean: Vec<bool>,
}

impl Health {
    fn new<'t>(model: &Model, traces: impl Iterator<Item = &'t Trace>) -> Self {
        let mut bad = vec![false; model.states.len()];
        for trace in traces {
            if let Some(&end) = trace.states.last() {
                bad[end] = true;
            }
        }
        let clean = clean_states(model, &bad);
        Self { bad, clean }
    }

    /// Upstream's `badKeys`: among the keys of this model's states, each that a bad state has,
    /// or that no clean state has.
    fn wrong(&self, key_of: &[usize], keys: usize) -> Vec<bool> {
        let mut present = vec![false; keys];
        let mut bad = vec![false; keys];
        let mut clean = vec![false; keys];
        for (state, &key) in key_of.iter().enumerate() {
            present[key] = true;
            bad[key] |= self.bad[state];
            clean[key] |= self.clean[state];
        }
        (0..keys)
            .map(|key| present[key] && (bad[key] || !clean[key]))
            .collect()
    }
}

/// The second model as the charts read it.
pub(super) struct SecondSide<'a> {
    pub(super) side: Side<'a>,
    pub(super) role: &'a Role,
    /// The variables and record fields the first model has and this one lacks.
    pub(super) lacks: Vec<Vec<String>>,
    /// Each state's variables without the fields that the first model lacks.
    pub(super) shared: Vec<Vars>,
    pub(super) actions: HashSet<Option<Arc<str>>>,
    mapping: Mapping,
    health: Health,
    /// For a fix: the first model's states that break the rules the fix does not check, which
    /// stay as wrong as they are.
    unchecked: Option<Health>,
}

impl<'a> SecondSide<'a> {
    fn new(
        model: &Model,
        second: &'a SecondModel<'_>,
        mapping: &Mapping,
    ) -> Result<Self, ChartError> {
        let label: Arc<str> = second.role.label().into();
        let (start, second_start) = (first_state(model), first_state(second.model));
        let lacks = second::lacking(&start, &second_start);
        let extra = second::lacking(&second_start, &start);
        let mapping = mapping.for_lacking_states();
        let vars = second
            .model
            .states
            .iter()
            .map(|state| mapping::lacking_value(state, &lacks, &label))
            .collect();
        let side = Side::new(second.model, &mapping, vars)?;
        let unchecked = second.role.is_fix().then(|| {
            Health::new(
                model,
                model
                    .traces
                    .iter()
                    .filter(|trace| !second.model.rules.contains(&trace.rule)),
            )
        });
        Ok(Self {
            shared: second
                .model
                .states
                .iter()
                .map(|state| second::omit(state, &extra))
                .collect(),
            actions: second
                .model
                .steps
                .iter()
                .map(|step| step.action.clone())
                .collect(),
            health: Health::new(second.model, second.model.traces.iter()),
            side,
            role: &second.role,
            lacks,
            mapping,
            unchecked,
        })
    }

    /// "with the fix, LABEL" or "in the spec, LABEL".
    pub(super) fn within(&self, label: &str) -> String {
        format!("{} {}, {label}", self.role.within(), self.role.label())
    }
}

fn first_state(model: &Model) -> Vars {
    model
        .initial
        .first()
        .map(|&state| model.states[state].clone())
        .unwrap_or_default()
}

/// What every chart of one run shares: the mapping's words for each state and step of both
/// models, read once, and the bugs.
pub struct Charter<'a> {
    pub(super) model: &'a Model,
    pub(super) mapping: &'a Mapping,
    pub(super) rules: &'a HashMap<String, String>,
    pub(super) main: Side<'a>,
    health: Health,
    pub(super) second: Option<SecondSide<'a>>,
    /// The model's bugs, then those only the second model has.
    pub(super) bugs: Vec<Bug>,
    pub(super) legend: [String; 3],
    pub(super) palette: Palette,
}

impl<'a> Charter<'a> {
    pub fn new(
        model: &'a Model,
        mapping: &'a Mapping,
        rules: &'a HashMap<String, String>,
    ) -> Result<Self, ChartError> {
        Self::with_second(model, None, mapping, rules)
    }

    /// The charter of a model and the second model each bug replays on. The mapping labels
    /// both, and fails when it reads a variable or field that the second model lacks.
    pub fn with_second(
        model: &'a Model,
        second: Option<&'a SecondModel<'_>>,
        mapping: &'a Mapping,
        rules: &'a HashMap<String, String>,
    ) -> Result<Self, ChartError> {
        let values = model.states.iter().map(mapping::value).collect();
        let main = Side::new(model, mapping, values)?;
        let second = second
            .map(|second| SecondSide::new(model, second, mapping))
            .transpose()?;
        let legend = legend(mapping, second.as_ref().map(|second| second.role));
        let palette = if mapping.has_group() {
            let groups: Vec<String> = main
                .groups
                .iter()
                .chain(second.iter().flat_map(|second| &second.side.groups))
                .cloned()
                .collect();
            palette(mapping, &groups, &legend)?
        } else {
            Palette::default()
        };
        let mut charter = Self {
            model,
            mapping,
            rules,
            health: Health::new(model, model.traces.iter()),
            main,
            second,
            bugs: Vec::new(),
            legend,
            palette,
        };
        charter.bugs = charter.all_bugs()?;
        Ok(charter)
    }

    /// Draws one chart.
    pub fn chart(&self, options: &ChartOptions) -> Result<Drawing, ChartError> {
        if options.regions
            && (options.view != View::Customer
                || options.flat
                || options.path_only
                || options.highlight != 0)
        {
            return Err(ChartError::RegionsOptions);
        }
        let chart = Chart::new(self, options)?;
        let regions = if options.regions {
            chart.regions()?
        } else {
            None
        };
        let flat = chart.flat();
        let drawn = regions.as_ref().and_then(|r| r.drawn.as_ref().ok());
        Ok(Drawing {
            mermaid: drawn.map_or_else(|| flat.mermaid.clone(), |d| d.mermaid.clone()),
            stories: story::stories(&chart),
            summary: story::summary(&chart, &flat, regions.as_ref()),
            regions_proof: regions.as_ref().and_then(|r| r.proof.clone()),
            regions_all: drawn.map(|d| d.all_mermaid.clone()),
        })
    }

    pub(super) fn is_grouping(&self) -> bool {
        self.mapping.has_group()
    }

    pub(super) fn role(&self) -> Option<&Role> {
        self.second.as_ref().map(|second| second.role)
    }

    /// The counterexample behind a bug, of the model that has it.
    pub(super) fn trace(&self, bug: &Bug) -> &Trace {
        match (&self.second, bug.introduced) {
            (Some(second), true) => &second.side.model.traces[bug.trace],
            _ => &self.model.traces[bug.trace],
        }
    }

    pub(super) fn keys(&self, view: View) -> Result<Keys, ChartError> {
        if view != View::Customer && !self.mapping.has_detail() {
            return Err(ChartError::NoDetail(view.name()));
        }
        let mut keys = Keys::default();
        let mut known: HashMap<String, usize> = HashMap::new();
        for state in 0..self.model.states.len() {
            let key = keys.intern(&mut known, self.main.name(state, view));
            keys.of.push(key);
        }
        keys.main = keys.names.len();
        if let Some(second) = &self.second {
            for state in 0..second.side.model.states.len() {
                let key = keys.intern(&mut known, second.side.name(state, view));
                keys.second.push(key);
            }
        }
        Ok(keys)
    }

    /// Where the reader's view moves along `trace`: its first state, and each step that changes
    /// the reader's state or that the reader takes, as upstream's `readerPath`.
    pub(super) fn reader_steps(&self, trace: &Trace, key_of: &[usize]) -> Vec<usize> {
        let mut kept = vec![0];
        for at in 1..trace.states.len() {
            let action = trace.actions[at - 1].as_deref().unwrap_or_default();
            let previous = key_of[trace.states[kept[kept.len() - 1]]];
            if key_of[trace.states[at]] == previous && !self.mapping.reader_actions.contains(action)
            {
                continue;
            }
            kept.push(at);
        }
        kept
    }

    /// The reader's path along a trace of the model, or of `second`.
    pub(super) fn reader_path(
        &self,
        trace: &Trace,
        key_of: &[usize],
        second: Option<&SecondSide>,
        view: View,
    ) -> Result<Vec<PathStep>, ChartError> {
        let (side, mapping) = match second {
            Some(second) => (&second.side, &second.mapping),
            None => (&self.main, self.mapping),
        };
        let mut path = Vec::new();
        for at in self.reader_steps(trace, key_of) {
            let key = key_of[trace.states[at]];
            if at == 0 {
                path.push(PathStep {
                    key,
                    label: String::new(),
                    guard: String::new(),
                });
                continue;
            }
            let action = trace.actions[at - 1].clone().unwrap_or_default();
            let words =
                side.step_words(mapping, trace.states[at - 1], trace.states[at], &action)?;
            path.push(PathStep {
                key,
                label: or_action(words.label(view, &action), &action),
                guard: words.guard,
            });
        }
        Ok(path)
    }

    /// The model's bugs, each with its replay on the second model, then the bugs that only the
    /// second model has.
    fn all_bugs(&self) -> Result<Vec<Bug>, ChartError> {
        let keys = self.keys(View::Customer)?;
        let mut bugs = self.side_bugs(&self.main, self.mapping, &keys.of, &keys.names, false)?;
        let Some(second) = &self.second else {
            return Ok(bugs);
        };
        for bug in &mut bugs {
            bug.replay = Some(self.replay(second, &self.model.traces[bug.trace], &keys)?);
        }
        let known: HashSet<String> = bugs.iter().map(|bug| bug.id.clone()).collect();
        let introduced = self.side_bugs(
            &second.side,
            &second.mapping,
            &keys.second,
            &keys.names,
            true,
        )?;
        bugs.extend(
            introduced
                .into_iter()
                .filter(|bug| !known.contains(&bug.id)),
        );
        Ok(bugs)
    }

    /// Upstream's `group`: counterexamples of one rule with one cause, or one end state, are one
    /// bug, drawn by its shortest reader path; bugs order by fewest reader steps.
    fn side_bugs(
        &self,
        side: &Side,
        mapping: &Mapping,
        key_of: &[usize],
        names: &[String],
        introduced: bool,
    ) -> Result<Vec<Bug>, ChartError> {
        let traces = &side.model.traces;
        let mut by_id: OrderedMap<String, Bug> = OrderedMap::default();
        for (index, trace) in traces.iter().enumerate() {
            let kept = self.reader_steps(trace, key_of);
            let end = trace.states[trace.states.len() - 1];
            let cause = mapping.cause(&side.vars[end], &side.facts[end], &trace.rule)?;
            let shown = cause
                .clone()
                .unwrap_or_else(|| names[key_of[trace.states[kept[kept.len() - 1]]]].clone());
            let id = format!("{}{SEP}{shown}", trace.rule);
            let steps = kept.len();
            let bug = |count| Bug {
                trace: index,
                cause: cause.clone(),
                count,
                introduced,
                replay: None,
                steps,
                id: id.clone(),
            };
            match by_id.get_mut(&id) {
                Some(found)
                    if steps < found.steps
                        || (steps == found.steps
                            && trace.states.len() < traces[found.trace].states.len()) =>
                {
                    *found = bug(found.count + 1);
                }
                Some(found) => found.count += 1,
                None => by_id.insert(id.clone(), bug(1)),
            }
        }
        let mut bugs: Vec<Bug> = by_id.into_values();
        bugs.sort_by(|a, b| {
            a.steps
                .cmp(&b.steps)
                .then_with(|| {
                    traces[a.trace]
                        .states
                        .len()
                        .cmp(&traces[b.trace].states.len())
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(bugs)
    }

    /// Each key's status: with a fix, a state that the fix no longer breaks is fixed; against a
    /// spec, a state that either breaks is wrong. A state only the second model has is fixed.
    fn status(&self, keys: &Keys) -> Vec<Status> {
        let count = keys.names.len();
        let now = self.health.wrong(&keys.of, count);
        let Some(second) = &self.second else {
            return now
                .into_iter()
                .map(|wrong| if wrong { Status::Wrong } else { Status::Works })
                .collect();
        };
        let mut later = second.health.wrong(&keys.second, count);
        if let Some(unchecked) = &second.unchecked {
            for (key, wrong) in unchecked.wrong(&keys.of, count).into_iter().enumerate() {
                later[key] |= wrong;
            }
        }
        (0..count)
            .map(|key| {
                let only_second = key >= keys.main;
                match (second.role, now[key], later[key]) {
                    (Role::Fix { .. }, _, true) | (Role::Against { .. }, true, _) => Status::Wrong,
                    (Role::Against { .. }, _, true) => Status::Wrong,
                    (Role::Fix { .. }, true, _) => Status::Fixed,
                    _ if only_second => Status::Fixed,
                    _ => Status::Works,
                }
            })
            .collect()
    }

    /// Upstream's `divergence`: where the second model takes the reader at a bug's turn, as an
    /// arrow of this view.
    fn divergence(
        &self,
        turn: &Turn,
        keys: &Keys,
        edges: &OrderedMap<EdgeId, Edge>,
        view: View,
    ) -> Result<Option<Divergence>, ChartError> {
        let Some(second) = &self.second else {
            return Ok(None);
        };
        let from = keys.of[turn.before];
        let (key, label) = match turn.taken {
            None => {
                let words =
                    self.main
                        .step_words(self.mapping, turn.before, turn.after, &turn.action)?;
                (
                    from,
                    or_action(words.label(view, &turn.action), &turn.action),
                )
            }
            Some(index) => {
                let step = &second.side.model.steps[index];
                let action = step.action.clone().unwrap_or_default();
                let label = second.side.words[index].label(view, &action);
                (keys.second[step.to], or_action(label, &action))
            }
        };
        let disabled = turn.taken.is_none();
        let text = second.within(&format!(
            "{label}{}",
            if disabled { CANNOT_HAPPEN } else { "" }
        ));
        let plain = (from, key, false);
        let drawn = !disabled
            && edges
                .get(&plain)
                .is_some_and(|edge| edge.labels.contains(&label) || edge.labels.contains(&text));
        Ok(Some(Divergence {
            from,
            key,
            disabled,
            text,
            edge: if drawn { plain } else { (from, key, true) },
        }))
    }
}

/// A step's label, or its action when the mapping gives it no words.
fn or_action(label: String, action: &str) -> String {
    if label.is_empty() {
        action.to_owned()
    } else {
        label
    }
}

/// The legend's words for each status: the mapping's, or upstream's for the second model's role.
fn legend(mapping: &Mapping, role: Option<&Role>) -> [String; 3] {
    let label = role.map_or(DEFAULT_AGAINST_LABEL, Role::label);
    let fix = role.is_some_and(Role::is_fix);
    let given = &mapping.legend;
    [
        given
            .works
            .clone()
            .unwrap_or_else(|| if fix { FIX_WORKS } else { DEFAULT_WORKS }.to_owned()),
        given.wrong.clone().unwrap_or_else(|| {
            if fix {
                format!("Still wrong with {label}")
            } else {
                DEFAULT_WRONG.to_owned()
            }
        }),
        given.fixed.clone().unwrap_or_else(|| {
            if fix {
                format!("Fixed or new with {label}")
            } else {
                format!("Only in {label}")
            }
        }),
    ]
}

/// The edges between the reader states of one model's steps, each label once, the most common
/// first.
fn quotient(side: &Side, key_of: &[usize], view: View) -> OrderedMap<EdgeId, Edge> {
    let mut counts: OrderedMap<EdgeId, LabelSteps> = OrderedMap::default();
    for (index, step) in side.model.steps.iter().enumerate() {
        let (from, to) = (key_of[step.from], key_of[step.to]);
        if step.from == step.to || from == to {
            continue;
        }
        let (labels, guards) = counts.entry((from, to, false), Default::default);
        let words = &side.words[index];
        let label = words.label(view, step.action.as_deref().unwrap_or_default());
        if label.is_empty() {
            continue;
        }
        *labels.entry(label.clone(), || 0) += 1;
        guards.entry(label).or_default().insert(words.guard.clone());
    }
    let mut edges: OrderedMap<EdgeId, Edge> = OrderedMap::default();
    for (&id, (labels, guards)) in counts.iter() {
        let mut ordered: Vec<(&String, &usize)> = labels.iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(a.1));
        edges.insert(
            id,
            Edge {
                from: id.0,
                to: id.1,
                labels: ordered
                    .into_iter()
                    .map(|(label, _)| label.clone())
                    .collect(),
                guards: Some(guards.clone()),
                second_only: false,
            },
        );
    }
    edges
}

fn step_words(
    mapping: &Mapping,
    action: &str,
    before: &Dynamic,
    after: &Dynamic,
) -> Result<StepWords, MappingError> {
    Ok(StepWords {
        said: mapping.step(action, before, after)?,
        seconds: mapping.timer(action, before, after)?,
        guard: mapping.guard(action, before, after)?,
    })
}

/// The states reached from a good start state through good states only.
fn clean_states(model: &Model, bad: &[bool]) -> Vec<bool> {
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); model.states.len()];
    for step in model.steps.iter().filter(|step| step.from != step.to) {
        next[step.from].push(step.to);
    }
    let mut clean = vec![false; model.states.len()];
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &start in &model.initial {
        if !bad[start] && !clean[start] {
            clean[start] = true;
            queue.push_back(start);
        }
    }
    while let Some(from) = queue.pop_front() {
        for &to in &next[from] {
            if !clean[to] && !bad[to] {
                clean[to] = true;
                queue.push_back(to);
            }
        }
    }
    clean
}

/// The legend's groups, in the mapping's `group_colors` order and then from A to Z, each with
/// its fill.
fn palette(
    mapping: &Mapping,
    groups: &[String],
    legend: &[String; 3],
) -> Result<Palette, ChartError> {
    let status_names: HashSet<String> = STATUS_GROUP_NAMES
        .iter()
        .copied()
        .chain(legend.iter().map(String::as_str))
        .map(str::to_lowercase)
        .collect();
    let found: BTreeSet<&str> = groups
        .iter()
        .map(String::as_str)
        .filter(|g| !g.is_empty())
        .collect();
    if let Some(group) = found
        .iter()
        .find(|g| status_names.contains(&g.to_lowercase()))
    {
        return Err(ChartError::StatusGroup((*group).to_owned()));
    }
    let given: OrderedMap<&str, Option<&str>> =
        mapping
            .group_colors
            .iter()
            .fold(OrderedMap::default(), |mut given, (group, color)| {
                given.insert(group.as_str(), color.as_deref());
                given
            });
    let mut names: Vec<String> = given
        .keys()
        .filter(|group| found.contains(*group))
        .map(|group| (*group).to_owned())
        .collect();
    names.extend(
        found
            .iter()
            .filter(|group| !given.contains_key(group))
            .map(|group| (*group).to_owned()),
    );
    let mut fills = HashMap::new();
    for (group, color) in given.iter() {
        let Some(color) = color.filter(|color| !color.is_empty()) else {
            continue;
        };
        if let Some(problem) = group_color_problem(color) {
            return Err(ChartError::GroupColor {
                group: (*group).to_owned(),
                color: color.to_owned(),
                problem,
            });
        }
        if found.contains(group) {
            fills.insert((*group).to_owned(), color.to_lowercase());
        }
    }
    let taken: HashSet<String> = fills.values().cloned().collect();
    let mut free = GROUP_PALETTE
        .iter()
        .filter(|color| !taken.contains(**color));
    for group in &names {
        if fills.contains_key(group) {
            continue;
        }
        let color = free.next().ok_or(ChartError::Palette(names.len()))?;
        fills.insert(group.clone(), (*color).to_owned());
    }
    Ok(Palette { names, fills })
}

/// The reader states of one view: each state's key, and each key's label, in the order the
/// model first reaches them, then the second model.
#[derive(Debug, Default)]
pub(super) struct Keys {
    pub(super) names: Vec<String>,
    pub(super) of: Vec<usize>,
    /// The keys of the second model's states.
    pub(super) second: Vec<usize>,
    /// The keys from this one on are only the second model's.
    pub(super) main: usize,
}

impl Keys {
    fn intern(&mut self, known: &mut HashMap<String, usize>, name: String) -> usize {
        *known.entry(name).or_insert_with_key(|name| {
            self.names.push(name.clone());
            self.names.len() - 1
        })
    }
}

/// An edge between two keys. A second edge between the same keys, `true`, says where the second
/// model takes a bug that it does not take as the chart draws it.
pub(super) type EdgeId = (usize, usize, bool);

#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) labels: OrderedSet<String>,
    /// The mapping's guard words for each label, for edges of the models' own steps.
    pub(super) guards: Option<HashMap<String, BTreeSet<String>>>,
    /// Only the second model takes this edge.
    pub(super) second_only: bool,
}

#[derive(Debug, Clone)]
pub(super) struct PathStep {
    pub(super) key: usize,
    pub(super) label: String,
    pub(super) guard: String,
}

pub(super) struct Class<'b> {
    pub(super) bug: &'b Bug,
    pub(super) path: Vec<PathStep>,
    /// Where the second model takes the reader instead, when it parts from the bug's path.
    pub(super) divergence: Option<Divergence>,
}

impl Class<'_> {
    pub(super) fn end(&self) -> usize {
        self.path[self.path.len() - 1].key
    }

    /// Each reader step along the path, as an edge of the chart.
    pub(super) fn steps(&self) -> impl Iterator<Item = (EdgeId, &PathStep)> {
        self.path
            .windows(2)
            .map(|pair| ((pair[0].key, pair[1].key, false), &pair[1]))
    }
}

/// Where the second model takes a bug at its turn, as an arrow of one view.
pub(super) struct Divergence {
    pub(super) from: usize,
    pub(super) key: usize,
    pub(super) disabled: bool,
    /// The arrow's words: "with the fix, STEP", or "... STEP cannot happen".
    pub(super) text: String,
    pub(super) edge: EdgeId,
}

struct Numbered {
    n: usize,
    label: String,
    shown: String,
}

#[derive(Debug, Clone)]
pub(super) struct DrawnState {
    pub(super) group: String,
    pub(super) status: Status,
    pub(super) class: String,
}

/// What the flat chart drew, which the summary counts even when regions replace it.
pub(super) struct Flat {
    pub(super) mermaid: String,
    pub(super) states: Vec<usize>,
    pub(super) transitions: usize,
    pub(super) drawn: Vec<DrawnState>,
    pub(super) labels: OrderedSet<String>,
}

/// One arrow's lines: the numbered steps, then the model's steps, then the second model's.
struct Arrow {
    from: usize,
    to: usize,
    groups: [Vec<String>; 3],
    plain: bool,
    labels: Vec<String>,
}

struct Line {
    from: usize,
    to: usize,
    text: String,
    plain: bool,
    labels: Vec<String>,
}

/// The edges of one model's steps, where it starts, and the settled states whose shortest
/// paths the cover keeps.
struct Reach<'r> {
    edges: &'r OrderedMap<EdgeId, Edge>,
    initial: Vec<usize>,
    settled: Vec<usize>,
}

/// One chart: the model collapsed onto the reader's states of one view.
pub(super) struct Chart<'c> {
    pub(super) charter: &'c Charter<'c>,
    pub(super) options: &'c ChartOptions,
    pub(super) keys: Keys,
    pub(super) initial: OrderedSet<usize>,
    pub(super) edges: OrderedMap<EdgeId, Edge>,
    path_numbers: HashMap<EdgeId, Vec<Numbered>>,
    cover: Option<OrderedSet<EdgeId>>,
    pub(super) classes: Vec<Class<'c>>,
    status: Vec<Status>,
    groups_of: Vec<OrderedSet<String>>,
    chosen: Option<usize>,
    on_path: HashSet<usize>,
}

impl<'c> Chart<'c> {
    fn new(charter: &'c Charter<'c>, options: &'c ChartOptions) -> Result<Self, ChartError> {
        let view = options.view;
        let keys = charter.keys(view)?;
        let main_edges = quotient(&charter.main, &keys.of, view);
        let second_edges = charter
            .second
            .as_ref()
            .map(|second| quotient(&second.side, &keys.second, view));
        let mut edges = main_edges.clone();
        if let (Some(second), Some(second_edges)) = (&charter.second, &second_edges) {
            for (&id, edge) in second_edges.iter() {
                if edges.contains_key(&id) {
                    continue;
                }
                let guards = edge.guards.as_ref().map(|guards| {
                    guards
                        .iter()
                        .map(|(label, words)| (second.within(label), words.clone()))
                        .collect()
                });
                edges.insert(
                    id,
                    Edge {
                        from: edge.from,
                        to: edge.to,
                        labels: edge
                            .labels
                            .iter()
                            .map(|label| second.within(label))
                            .collect(),
                        guards,
                        second_only: true,
                    },
                );
            }
        }
        let main_initial: Vec<usize> = charter.model.initial.iter().map(|&s| keys.of[s]).collect();
        let second_initial: Vec<usize> = charter
            .second
            .iter()
            .flat_map(|second| second.side.model.initial.iter().map(|&s| keys.second[s]))
            .collect();
        let initial: OrderedSet<usize> = main_initial
            .iter()
            .chain(&second_initial)
            .copied()
            .collect();
        let mut groups_of = vec![OrderedSet::default(); keys.names.len()];
        for (state, group) in charter.main.groups.iter().enumerate() {
            groups_of[keys.of[state]].insert(group.clone());
        }
        if let Some(second) = &charter.second {
            for (state, group) in second.side.groups.iter().enumerate() {
                groups_of[keys.second[state]].insert(group.clone());
            }
        }
        let mut classes = Vec::with_capacity(charter.bugs.len());
        for bug in &charter.bugs {
            let (key_of, second) = if bug.introduced {
                (&keys.second, charter.second.as_ref())
            } else {
                (&keys.of, None)
            };
            let divergence = match &bug.replay {
                Some(Replay::Differs(turn)) => charter.divergence(turn, &keys, &edges, view)?,
                _ => None,
            };
            classes.push(Class {
                bug,
                path: charter.reader_path(charter.trace(bug), key_of, second, view)?,
                divergence,
            });
        }
        let chosen = options.highlight.checked_sub(1);
        if let Some(index) = chosen
            && index >= classes.len()
            && !classes.is_empty()
        {
            return Err(ChartError::NoSuchBug {
                highlight: options.highlight,
                count: classes.len(),
            });
        }
        let chosen = chosen.filter(|&index| index < classes.len());
        let on_path = chosen
            .map(|index| {
                let class = &classes[index];
                class
                    .path
                    .iter()
                    .map(|step| step.key)
                    .chain(class.divergence.as_ref().map(|d| d.key))
                    .collect()
            })
            .unwrap_or_default();
        let settled = |side: &Side, key_of: &[usize]| -> Vec<usize> {
            (0..side.model.states.len())
                .filter(|&state| side.settled[state])
                .map(|state| key_of[state])
                .collect()
        };
        let mut reaches = vec![Reach {
            edges: &main_edges,
            initial: main_initial,
            settled: settled(&charter.main, &keys.of),
        }];
        if let (Some(second), Some(second_edges)) = (&charter.second, &second_edges) {
            reaches.push(Reach {
                edges: second_edges,
                initial: second_initial,
                settled: settled(&second.side, &keys.second)
                    .into_iter()
                    .filter(|&key| key >= keys.main)
                    .collect(),
            });
        }
        let mut chart = Self {
            charter,
            options,
            status: charter.status(&keys),
            keys,
            initial,
            edges,
            path_numbers: HashMap::new(),
            cover: None,
            classes,
            groups_of,
            chosen,
            on_path,
        };
        chart.add_path_and_cover(&reaches);
        Ok(chart)
    }

    pub(super) fn name(&self, key: usize) -> &str {
        &self.keys.names[key]
    }

    pub(super) fn status(&self, key: usize) -> Status {
        self.status[key]
    }

    /// A key as the stories name it: a layered key reads "state: detail".
    pub(super) fn show(&self, key: usize) -> String {
        let name = self.name(key);
        if self.options.view == View::Layered {
            name.replacen(SEP, CONTEXT, 1)
        } else {
            name.to_owned()
        }
    }

    pub(super) fn group_of(&self, key: usize) -> &str {
        match self.groups_of[key].iter().collect::<Vec<_>>().as_slice() {
            [only] => only,
            _ => "",
        }
    }

    pub(super) fn mixed_groups(&self) -> Vec<String> {
        let mut mixed: Vec<String> = (0..self.keys.names.len())
            .filter(|&key| self.groups_of[key].len() > 1)
            .map(|key| self.show(key))
            .collect();
        mixed.sort();
        mixed
    }

    pub(super) fn legend_words(&self, status: Status) -> &str {
        &self.charter.legend[status as usize]
    }

    /// Upstream's `chartEdges`: the arrows of each bug's divergence, the highlighted bug's
    /// numbered steps, and with `cover` only the shortest path to each settled state of each
    /// model and every bug's path.
    fn add_path_and_cover(&mut self, reaches: &[Reach]) {
        for divergence in self.classes.iter().filter_map(|c| c.divergence.as_ref()) {
            if !divergence.edge.2 {
                continue;
            }
            self.edges
                .entry(divergence.edge, || Edge {
                    from: divergence.from,
                    to: divergence.key,
                    labels: OrderedSet::default(),
                    guards: None,
                    second_only: true,
                })
                .labels
                .insert(divergence.text.clone());
        }
        if let Some(chosen) = self.chosen {
            let along: Vec<(EdgeId, PathStep)> = self.classes[chosen]
                .steps()
                .map(|(id, step)| (id, step.clone()))
                .collect();
            for (index, (id, step)) in along.into_iter().enumerate() {
                let shown = if step.guard.is_empty() {
                    step.label.clone()
                } else {
                    format!("{} [{}]", step.label, step.guard)
                };
                self.path_numbers.entry(id).or_default().push(Numbered {
                    n: index + 1,
                    label: step.label.clone(),
                    shown,
                });
                if !self.edges.contains_key(&id) {
                    self.edges.insert(
                        id,
                        Edge {
                            from: id.0,
                            to: id.1,
                            labels: [step.label].into_iter().collect(),
                            guards: None,
                            second_only: false,
                        },
                    );
                }
            }
        }
        if !self.options.cover {
            return;
        }
        let mut cover: OrderedSet<EdgeId> = OrderedSet::default();
        for reach in reaches {
            cover_paths(reach, &mut cover);
        }
        let mut path_labels: HashMap<EdgeId, OrderedSet<String>> = HashMap::new();
        for class in &self.classes {
            for (id, step) in class.steps() {
                cover.insert(id);
                path_labels
                    .entry(id)
                    .or_default()
                    .insert(step.label.clone());
            }
            if let Some(divergence) = &class.divergence {
                cover.insert(divergence.edge);
            }
        }
        for &id in cover.iter() {
            if !self.edges.contains_key(&id) {
                self.edges.insert(
                    id,
                    Edge {
                        from: id.0,
                        to: id.1,
                        labels: path_labels.remove(&id).unwrap_or_default(),
                        guards: None,
                        second_only: false,
                    },
                );
            }
        }
        self.cover = Some(cover);
    }

    pub(super) fn drawn_state(&self, group: &str, status: Status, path: bool) -> DrawnState {
        let class = if self.charter.is_grouping() {
            let base = match self.charter.palette.names.iter().position(|g| g == group) {
                Some(index) if !group.is_empty() => format!("group{index}"),
                _ => PLAIN.to_owned(),
            };
            let border = match (status.border(), path) {
                ("", true) => PATH,
                (border, _) => border,
            };
            format!("{base}{border}")
        } else {
            format!("{}{}", status.name(), if path { PATH } else { "" })
        };
        DrawnState {
            group: group.to_owned(),
            status,
            class,
        }
    }

    pub(super) fn group_counts(&self, drawn: &[DrawnState]) -> Vec<(String, String, usize)> {
        let palette = &self.charter.palette;
        palette
            .names
            .iter()
            .map(|group| {
                (
                    group.clone(),
                    palette.fills.get(group).cloned().unwrap_or_default(),
                    drawn.iter().filter(|d| d.group == *group).count(),
                )
            })
            .filter(|(.., states)| *states > 0)
            .collect()
    }

    /// The front matter, classes and legend every chart starts with.
    pub(super) fn head(&self, drawn: &[DrawnState]) -> Vec<String> {
        let mut out = front_matter(self.options.title.as_deref());
        out.push("stateDiagram-v2".to_owned());
        if let Some(role) = self.charter.role() {
            out.push(format!(
                "  %% second model ({}): {}",
                role.name(),
                role.label()
            ));
        }
        if self.options.full_labels {
            out.push(format!("  {FULL_LABELS}"));
        }
        let used: Vec<Status> = Status::ALL
            .into_iter()
            .filter(|status| drawn.iter().any(|d| d.status == *status))
            .collect();
        let legend: Vec<(String, String, String)> = if self.charter.is_grouping() {
            self.group_counts(drawn)
                .into_iter()
                .map(|(name, ..)| {
                    let index = self
                        .charter
                        .palette
                        .names
                        .iter()
                        .position(|g| *g == name)
                        .unwrap_or_default();
                    (format!("legendGroup{index}"), name, format!("group{index}"))
                })
                .chain(
                    used.iter()
                        .filter(|status| !status.border().is_empty())
                        .map(|status| {
                            (
                                status.legend_id().to_owned(),
                                self.legend_words(*status).to_owned(),
                                format!("{PLAIN}{}", status.border()),
                            )
                        }),
                )
                .collect()
        } else {
            used.iter()
                .map(|status| {
                    (
                        status.legend_id().to_owned(),
                        self.legend_words(*status).to_owned(),
                        status.name().to_owned(),
                    )
                })
                .collect()
        };
        if self.charter.is_grouping() {
            let classes: HashSet<&str> = drawn
                .iter()
                .map(|d| d.class.as_str())
                .chain(legend.iter().map(|(.., class)| class.as_str()))
                .collect();
            let palette = &self.charter.palette;
            let bases = [(PLAIN.to_owned(), PLAIN_FILL.to_owned())]
                .into_iter()
                .chain(palette.names.iter().enumerate().map(|(index, group)| {
                    (
                        format!("group{index}"),
                        palette.fills.get(group).cloned().unwrap_or_default(),
                    )
                }));
            for (base, fill) in bases {
                for border in BORDERS {
                    if classes.contains(format!("{base}{border}").as_str()) {
                        out.push(format!("  {}", group_class_def(&base, &fill, border)));
                    }
                }
            }
        } else {
            out.extend(STATUS_CLASSES.iter().map(|class| format!("  {class}")));
        }
        if !legend.is_empty() {
            out.push(format!("  state \"Legend\" as {LEGEND_ID} {{"));
            for (id, text, _) in &legend {
                out.push(format!("    state \"{}\" as {id}", escape_quotes(text)));
            }
            out.push("  }".to_owned());
            for (id, _, class) in &legend {
                out.push(format!("  class {id} {class}"));
            }
        }
        out
    }

    /// The chart without regions: each reader state once, in boxes by context, and at most one
    /// arrow between two states.
    pub(super) fn flat(&self) -> Flat {
        let options = self.options;
        let chosen = self.chosen.map(|index| &self.classes[index]);
        let flat_states: HashSet<usize> = match (&self.cover, chosen) {
            (_, Some(_)) if options.path_only => self.on_path.clone(),
            (Some(cover), _) => self
                .initial
                .iter()
                .copied()
                .chain(
                    cover
                        .iter()
                        .filter_map(|id| self.edges.get(id))
                        .flat_map(|e| [e.from, e.to]),
                )
                .collect(),
            _ => (0..self.keys.names.len()).collect(),
        };
        let mut sorted: Vec<usize> = flat_states.iter().copied().collect();
        sorted.sort_by(|a, b| self.name(*a).cmp(self.name(*b)));
        let drawn: Vec<DrawnState> = sorted
            .iter()
            .map(|&key| {
                self.drawn_state(
                    self.group_of(key),
                    self.status(key),
                    self.on_path.contains(&key),
                )
            })
            .collect();

        let mut boxes: OrderedMap<String, Vec<usize>> = OrderedMap::default();
        match options.view {
            View::Layered => {
                for &key in &sorted {
                    let name = self.name(key);
                    let outer = name.split(SEP).next().unwrap_or(name);
                    boxes.entry(outer.to_owned(), Vec::new).push(key);
                }
            }
            View::Customer if !options.flat => {
                for &key in &sorted {
                    let name = self.name(key);
                    if let Some(at) = name.find(CONTEXT)
                        && at > 0
                        && at + CONTEXT.len() < name.len()
                    {
                        boxes.entry(name[..at].to_owned(), Vec::new).push(key);
                    }
                }
                boxes.retain(|_, members| members.len() >= 2);
            }
            _ => {}
        }
        let box_of: HashMap<usize, usize> = boxes
            .values()
            .enumerate()
            .flat_map(|(index, members)| members.iter().map(move |&key| (key, index)))
            .collect();
        let declared: Vec<usize> = boxes
            .values()
            .flatten()
            .copied()
            .chain(
                sorted
                    .iter()
                    .copied()
                    .filter(|key| !box_of.contains_key(key)),
            )
            .collect();
        let state_ids: HashMap<usize, String> = declared
            .iter()
            .enumerate()
            .map(|(index, &key)| (key, format!("s{index}")))
            .collect();
        let box_names: Vec<&String> = boxes.keys().collect();
        let caption = |key: usize| -> String {
            let name = self.name(key);
            match (options.view, box_of.get(&key)) {
                (View::Layered, _) => name.split(SEP).nth(1).unwrap_or_default().to_owned(),
                (_, Some(&index)) => name[box_names[index].len() + CONTEXT.len()..].to_owned(),
                _ => name.to_owned(),
            }
        };

        let chosen_turn = chosen.and_then(|class| class.divergence.as_ref());
        let mut labels = OrderedSet::default();
        let mut arrows: OrderedMap<(usize, usize), Arrow> = OrderedMap::default();
        for (id, edge) in self.edges.iter() {
            if !flat_states.contains(&edge.from) || !flat_states.contains(&edge.to) {
                continue;
            }
            let chosen_step = chosen_turn.filter(|turn| turn.edge == *id);
            if options.path_only
                && chosen.is_some()
                && !self.path_numbers.contains_key(id)
                && chosen_step.is_none()
            {
                continue;
            }
            if self.cover.as_ref().is_some_and(|cover| !cover.contains(id)) {
                continue;
            }
            let arrow = arrows.entry((edge.from, edge.to), || Arrow {
                from: edge.from,
                to: edge.to,
                groups: Default::default(),
                plain: true,
                labels: Vec::new(),
            });
            let all: Vec<&String> = edge.labels.iter().collect();
            let shown: Vec<String> = all
                .iter()
                .map(|label| guarded(edge.guards.as_ref().and_then(|g| g.get(*label)), label))
                .collect();
            let said = if options.full_labels {
                shown.join(OR)
            } else {
                join_labels(&shown)
            };
            match self.path_numbers.get(id) {
                Some(numbers) => {
                    arrow.groups[0].extend(numbered(numbers));
                    arrow.plain = false;
                    labels.extend(numbers.iter().map(|step| step.label.clone()));
                }
                None => {
                    match chosen_step {
                        Some(turn) if edge.second_only && options.path_only => {
                            arrow.groups[2].push(turn.text.clone());
                        }
                        _ if edge.second_only => arrow.groups[2].push(said),
                        _ => arrow.groups[1].push(said),
                    }
                    labels.extend(all.iter().map(|label| (*label).clone()));
                }
            }
            if let Some(turn) = chosen_step {
                if !edge.second_only {
                    arrow.groups[2].push(turn.text.clone());
                }
                arrow.plain = false;
            }
            if id.2 {
                arrow.plain = false;
            }
            arrow.labels.extend(all.into_iter().cloned());
        }
        let lines: Vec<Line> = arrows
            .into_values()
            .into_iter()
            .map(|arrow| {
                let mut sorted_labels = arrow.labels;
                sorted_labels.sort();
                Line {
                    from: arrow.from,
                    to: arrow.to,
                    text: arrow
                        .groups
                        .iter()
                        .map(|group| {
                            group
                                .iter()
                                .filter(|label| !label.is_empty())
                                .map(|label| {
                                    wrap(
                                        &label.replace(STATEMENT_END, STATEMENT_END_STANDIN),
                                        STEP_WIDTH,
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(BREAK)
                        })
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join(GROUP_GAP),
                    plain: arrow.plain,
                    labels: sorted_labels,
                }
            })
            .collect();

        let mut from_box: Vec<(usize, usize)> = Vec::new();
        let mut folded: HashSet<usize> = HashSet::new();
        for (box_index, members) in boxes.values().enumerate() {
            let mine: HashSet<usize> = members.iter().copied().collect();
            let mut by_step: OrderedMap<String, Vec<usize>> = OrderedMap::default();
            for (index, line) in lines.iter().enumerate() {
                if !line.plain || !mine.contains(&line.from) || mine.contains(&line.to) {
                    continue;
                }
                let step = format!(
                    "{}{SEP}{}{SEP}{}",
                    line.to,
                    line.text,
                    line.labels.join(&SEP.to_string())
                );
                by_step.entry(step, Vec::new).push(index);
            }
            for same in by_step.values() {
                let froms: HashSet<usize> = same.iter().map(|&index| lines[index].from).collect();
                if froms.len() != members.len() {
                    continue;
                }
                folded.extend(same.iter().copied());
                from_box.push((box_index, same[0]));
            }
        }

        let id = |key: usize| state_ids.get(&key).map_or("", String::as_str);
        let mut out = self.head(&drawn);
        for (index, (name, members)) in boxes.iter().enumerate() {
            out.push(format!(
                "  state \"{}\" as g{index} {{",
                escape_quotes(&wrap(name, STATE_WIDTH))
            ));
            for &key in members {
                out.push(format!(
                    "    state \"{}\" as {}",
                    escape_quotes(&wrap(&caption(key), STATE_WIDTH)),
                    id(key)
                ));
            }
            out.push("  }".to_owned());
        }
        for &key in sorted.iter().filter(|key| !box_of.contains_key(key)) {
            out.push(format!(
                "  state \"{}\" as {}",
                escape_quotes(&wrap(&caption(key), STATE_WIDTH)),
                id(key)
            ));
        }
        let first = chosen.map(|class| class.path[0].key);
        for &key in &sorted {
            if self.initial.contains(&key) && (!options.path_only || first == Some(key)) {
                out.push(format!("  [*] --> {}", id(key)));
            }
        }
        let arrow = |from: &str, line: &Line| {
            let text = if line.text.is_empty() {
                String::new()
            } else {
                format!(" : {}", line.text)
            };
            format!("  {from} --> {}{text}", id(line.to))
        };
        for &(box_index, line) in &from_box {
            out.push(arrow(&format!("g{box_index}"), &lines[line]));
        }
        for (index, line) in lines.iter().enumerate() {
            if !folded.contains(&index) {
                out.push(arrow(id(line.from), line));
            }
        }
        for (key, state) in sorted.iter().zip(&drawn) {
            out.push(format!("  class {} {}", id(*key), state.class));
        }
        Flat {
            mermaid: lines_text(&out),
            transitions: lines.len(),
            states: sorted,
            drawn,
            labels,
        }
    }
}

/// The lines of a chart file, each ending in a newline.
pub(super) fn lines_text(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The front matter that keeps Mermaid from shrinking the chart or rewrapping its labels.
pub(super) fn front_matter(title: Option<&str>) -> Vec<String> {
    let mut out = vec!["---".to_owned()];
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        out.push(format!("title: {}", Json::from(title)));
    }
    out.extend(
        [
            "config:",
            "  markdownAutoWrap: false",
            "  state:",
            "    useMaxWidth: false",
            "---",
        ]
        .map(str::to_owned),
    );
    out
}

/// A label with the mapping's guard words, when every model step behind it has some.
pub(super) fn guarded(words: Option<&BTreeSet<String>>, label: &str) -> String {
    match words {
        Some(words) if !words.contains("") => {
            format!(
                "{label} [{}]",
                words.iter().cloned().collect::<Vec<_>>().join(OR)
            )
        }
        _ => label.to_owned(),
    }
}

/// The shortest path to each settled state of one model, by its own steps.
fn cover_paths(reach: &Reach, cover: &mut OrderedSet<EdgeId>) {
    let mut out: HashMap<usize, Vec<EdgeId>> = HashMap::new();
    for &id in reach.edges.keys() {
        out.entry(id.0).or_default().push(id);
    }
    let mut parent: HashMap<usize, Option<EdgeId>> =
        reach.initial.iter().map(|&key| (key, None)).collect();
    let mut queue: VecDeque<usize> = reach.initial.iter().copied().collect();
    while let Some(key) = queue.pop_front() {
        for &id in out.get(&key).into_iter().flatten() {
            if parent.contains_key(&id.1) {
                continue;
            }
            parent.insert(id.1, Some(id));
            queue.push_back(id.1);
        }
    }
    for target in &reach.settled {
        let mut id = parent.get(target).copied().flatten();
        while let Some(edge) = id {
            cover.insert(edge);
            id = parent.get(&edge.0).copied().flatten();
        }
    }
}

/// "1, 2. you look": runs of numbered steps with the same words.
fn numbered(steps: &[Numbered]) -> Vec<String> {
    let mut runs: Vec<(&str, Vec<usize>)> = Vec::new();
    for step in steps {
        match runs.last_mut() {
            Some((shown, numbers))
                if *shown == step.shown && numbers.last() == Some(&(step.n - 1)) =>
            {
                numbers.push(step.n);
            }
            _ => runs.push((&step.shown, vec![step.n])),
        }
    }
    runs.into_iter()
        .map(|(shown, numbers)| {
            let numbers: Vec<String> = numbers.iter().map(usize::to_string).collect();
            format!("{}. {shown}", numbers.join(", "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use regex::Regex;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::scripts::statechart::mermaid::{compare, parse};
    use crate::scripts::statechart::model::{Step, explore};
    use crate::scripts::statechart::second::listed_breaks;
    use crate::scripts::statechart::testing::{
        CHARGE_AGAIN, PAY_CFG, PAY_MAP, Spec, arrows, explore_text, fixture, pay, rule_cfg,
        status_of, unwrapped_lines,
    };
    use crate::scripts::statechart::text::rule_sentences;

    const GOLD: &str = "gold";
    const CHART: &str = "chart.mmd";
    const STORIES: &str = "stories.md";
    const SUMMARY: &str = "summary.json";
    const REGIONS_ALL: &str = "regions-all.mmd";
    const REGIONS_PROOF: &str = "regions-proof.mmd";
    /// TLC names each step of an action with arguments by the action and its arguments, as
    /// "Start(1)", and tla-checker by the action alone. The mapping knows only "Start", so
    /// upstream's chart from TLC's graph shows "Start(1)" where the native chart shows the
    /// mapping's words, and counts different reader actions as settled. The OutputFolder golden
    /// is upstream's run on TLC's own graph with each step's arguments dropped from its name.
    const TLC_NAMES_STEPS_WITH_ARGUMENTS: &str = "OutputFolder";
    /// The regions proof lists the flat chart's steps in the order the search first took them,
    /// which TLC's and tla-checker's searches order differently; the steps are the same.
    const PROOF_LISTS_STEPS_IN_SEARCH_ORDER: &str = REGIONS_PROOF;
    /// Where several counterexamples are equally short, TLC and tla-checker may report different
    /// ones, and each bug chart and story follows its counterexample. The BrokenFolder golden is
    /// upstream's run on tla-checker's own graph and counterexamples.
    const UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES: &str = "BrokenFolder";
    const BROKEN_FOLDER_MAP: &str = "OutputFolder";
    const BROKEN_FOLDER_BUGS: usize = 50;
    const BROKEN_FOLDER_GOLDEN_BUGS: usize = 3;

    fn gold(name: &str, file: &str) -> Option<String> {
        fs::read_to_string(fixture(GOLD).join(name).join(file)).ok()
    }

    fn charts(name: &str, map: &str, comparing: bool) -> Charts {
        let spec = fixture(&format!("{name}.tla"));
        let model = explore(&spec, &fixture(&format!("{name}.cfg"))).expect("the model explores");
        let mapping =
            Mapping::load(&fixture(&format!("{map}.map.rhai"))).expect("the mapping parses");
        let options = DrawOptions {
            rules: rule_sentences(&fs::read_to_string(&spec).expect("the spec reads")),
            comparing,
            ..DrawOptions::default()
        };
        draw(&model, &mapping, &options).expect("the charts draw")
    }

    fn sorted_lines(text: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort_unstable();
        lines
    }

    fn summary_text(summary: &Json) -> String {
        format!("{}\n", serde_json::to_string_pretty(summary).expect("json"))
    }

    fn assert_gold(name: &str, file: &str, drawn: &str) {
        let expected = gold(name, file).unwrap_or_else(|| panic!("{name}/{file} has a golden"));
        assert_eq!(drawn, expected, "{name}/{file}");
    }

    #[test_case("Sync", true; "sync as from chart draws it back")]
    #[test_case("Upload", true; "upload as from chart draws it back")]
    #[test_case(TLC_NAMES_STEPS_WITH_ARGUMENTS, false; "output folder as from spec draws it")]
    fn draws_upstreams_golden(name: &str, comparing: bool) {
        let charts = charts(name, name, comparing);

        assert_gold(name, CHART, &charts.chart);
        assert_gold(name, STORIES, &charts.stories);
        assert_gold(name, SUMMARY, &summary_text(&charts.summary));
        let bugs: Vec<&str> = charts.bugs.iter().map(|bug| bug.file.as_str()).collect();
        let golden_bugs: Vec<String> = (1..=charts.bugs.len() + 1)
            .map(|n| format!("{BUG_FILE}{n}{MERMAID_EXTENSION}"))
            .filter(|file| gold(name, file).is_some())
            .collect();
        assert_eq!(bugs, golden_bugs);
        for bug in &charts.bugs {
            assert_gold(name, &bug.file, &bug.mermaid);
        }
        assert_eq!(charts.regions_all, gold(name, REGIONS_ALL));
        assert_eq!(
            charts.regions_proof.as_deref().map(sorted_lines),
            gold(name, PROOF_LISTS_STEPS_IN_SEARCH_ORDER)
                .as_deref()
                .map(sorted_lines)
        );
    }

    #[test]
    fn draws_upstreams_bugs() {
        let charts = charts(
            UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES,
            BROKEN_FOLDER_MAP,
            false,
        );

        assert_gold(UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES, CHART, &charts.chart);
        assert_gold(
            UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES,
            STORIES,
            &charts.stories,
        );
        assert_gold(
            UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES,
            SUMMARY,
            &summary_text(&charts.summary),
        );
        assert_eq!(charts.bugs.len(), BROKEN_FOLDER_BUGS);
        for n in 1..=BROKEN_FOLDER_GOLDEN_BUGS {
            let file = format!("{BUG_FILE}{n}{MERMAID_EXTENSION}");
            let bug = charts
                .bugs
                .iter()
                .find(|bug| bug.file == file)
                .unwrap_or_else(|| panic!("{file} is drawn"));
            assert_gold(UPSTREAM_DREW_NATIVE_COUNTEREXAMPLES, &file, &bug.mermaid);
        }
    }

    fn explored(spec: &str, cfg: &str) -> Model {
        explore(
            &fixture(&format!("{spec}.tla")),
            &fixture(&format!("{cfg}.cfg")),
        )
        .expect("the model explores")
    }

    fn assert_charts_gold(name: &str, charts: &Charts) {
        assert_gold(name, CHART, &charts.chart);
        assert_gold(name, STORIES, &charts.stories);
        let bugs: Vec<&str> = charts.bugs.iter().map(|bug| bug.file.as_str()).collect();
        let golden_bugs: Vec<String> = (1..=charts.bugs.len() + 1)
            .map(|n| format!("{BUG_FILE}{n}{MERMAID_EXTENSION}"))
            .filter(|file| gold(name, file).is_some())
            .collect();
        assert_eq!(bugs, golden_bugs);
        for bug in &charts.bugs {
            assert_gold(name, &bug.file, &bug.mermaid);
        }
    }

    fn second_charts(code: &str, second: &SecondModel) -> Charts {
        let main = explored(code, code);
        let spec = fixture(&format!("{code}.tla"));
        let mapping =
            Mapping::load(&fixture(&format!("{code}.map.rhai"))).expect("the mapping parses");
        let options = DrawOptions {
            rules: rule_sentences(&fs::read_to_string(&spec).expect("the spec reads")),
            ..DrawOptions::default()
        };
        draw_with(&main, Some(second), &mapping, &options).expect("the charts draw")
    }

    const SYNC_FIXED_GOLD: &str = "SyncFixed";
    const SAVE_AGREED_GOLD: &str = "SaveAgreed";
    const AGREED_LABEL: &str = "the agreed chart";
    const SUMMARY_SECOND: &str = "second";
    /// TLC writes a state's variables in its own order, here `retries, screen, note`, and
    /// tla-checker in the order the spec declares them. `second.lacks` follows that order, so the
    /// SaveAgreed golden lists `retries` before `note.rev`; the paths are the same.
    const TLC_ORDERS_A_STATES_VARIABLES: &str = "lacks";

    fn lacks_in_any_order(mut summary: Json) -> Json {
        if let Some(Json::Array(paths)) =
            summary[SUMMARY_SECOND].get_mut(TLC_ORDERS_A_STATES_VARIABLES)
        {
            paths.sort_by_key(Json::to_string);
        }
        summary
    }

    #[test]
    fn draws_upstreams_fix_golden() {
        let fixed = explored("SyncFixed", "Sync");
        let second = SecondModel {
            model: &fixed,
            role: Role::Fix { label: None },
        };

        let charts = second_charts("Sync", &second);

        assert_charts_gold(SYNC_FIXED_GOLD, &charts);
        assert_gold(SYNC_FIXED_GOLD, SUMMARY, &summary_text(&charts.summary));
    }

    #[test]
    fn draws_upstreams_against_golden() {
        let agreed = explored("SaveAgreed", "SaveCode");
        let listed = listed_breaks(&fixture("SaveAgreed.tla")).expect("the folder reads");
        let second = SecondModel {
            model: &agreed,
            role: Role::Against {
                label: Some(AGREED_LABEL.to_owned()),
                listed,
            },
        };

        let charts = second_charts("SaveCode", &second);

        assert_charts_gold(SAVE_AGREED_GOLD, &charts);
        let golden: Json = serde_json::from_str(
            &gold(SAVE_AGREED_GOLD, SUMMARY).expect("SaveAgreed has a summary golden"),
        )
        .expect("the golden is json");
        assert_eq!(
            lacks_in_any_order(charts.summary.clone()),
            lacks_in_any_order(golden)
        );
    }

    fn model(
        states: &[serde_json::Value],
        initial: &[usize],
        steps: &[(usize, usize, &str)],
    ) -> Model {
        Model {
            states: states
                .iter()
                .map(|state| state.as_object().expect("an object").clone())
                .collect(),
            initial: initial.to_vec(),
            steps: steps
                .iter()
                .map(|&(from, to, action)| Step {
                    from,
                    to,
                    action: Some(Arc::from(action)),
                })
                .collect(),
            rules: Vec::new(),
            traces: Vec::new(),
        }
    }

    fn one_chart(model: &Model, source: &str, options: &ChartOptions) -> Drawing {
        let mapping = Mapping::parse("Test.map.rhai", source).expect("the mapping parses");
        let rules = HashMap::new();
        Charter::new(model, &mapping, &rules)
            .expect("the charter reads the model")
            .chart(options)
            .expect("the chart draws")
    }

    const NAV_MAP: &str = r#"
        fn state(v) { v.s }
        fn step(a) {
            let words = #{ TapPay: "you tap Pay", TapBack: "you tap Back", TapHome: "you tap Home", OpenThread: "you open the thread", OpenApp: "you open the phone app", Finish: "" };
            words[a]
        }
    "#;
    const NATURAL_WIDTH: &str = "---\nconfig:\n  markdownAutoWrap: false\n  state:\n    useMaxWidth: false\n---\nstateDiagram-v2\n";

    fn nav() -> Model {
        let state = |s: &str, n: i64| json!({"s": s, "n": n});
        model(
            &[
                state("Home", 1),
                state("Home", 2),
                state("Home", 3),
                state("Away", 1),
                state("Away", 2),
                state("Done", 1),
            ],
            &[0],
            &[
                (0, 3, "TapPay"),
                (1, 4, "TapBack"),
                (2, 3, "TapHome"),
                (3, 0, "OpenThread"),
                (4, 1, "OpenApp"),
                (3, 5, "Finish"),
            ],
        )
    }

    #[test_case(false, &["Away -> Done : ", "Away -> Home : you open the thread or the phone app", "Home -> Away : you tap Pay, Back, or Home"]; "shared leading words once")]
    #[test_case(true, &["Away -> Done : ", "Away -> Home : you open the thread or you open the phone app", "Home -> Away : you tap Pay or you tap Back or you tap Home"]; "every label in full")]
    fn an_edge_lists_every_step(full_labels: bool, expected: &[&str]) {
        let drawing = one_chart(
            &nav(),
            NAV_MAP,
            &ChartOptions {
                full_labels,
                ..ChartOptions::default()
            },
        );

        assert!(
            drawing.mermaid.starts_with(NATURAL_WIDTH),
            "{}",
            drawing.mermaid
        );
        assert_eq!(drawing.mermaid.contains(FULL_LABELS), full_labels);
        assert_eq!(arrows(&drawing.mermaid), expected);
    }

    #[test]
    fn a_step_that_changes_nothing_keeps_a_state_unsettled_and_a_quote_survives() {
        let model = model(
            &[json!({"s": "Idle"}), json!({"s": "Saving"})],
            &[0],
            &[(0, 1, "Tap"), (1, 1, "Retry")],
        );
        let drawing = one_chart(
            &model,
            r#"
            fn reader_actions() { ["Tap"] }
            fn state(v, facts) { if v.s == "Idle" { "Badge says \"Synced\"" } else if facts.settled { "Saved" } else { "Saving" } }
            fn step(a) { if a == "Tap" { "you tap Save" } else { "" } }
            "#,
            &ChartOptions::default(),
        );

        assert_eq!(
            arrows(&drawing.mermaid),
            ["Badge says \"Synced\" -> Saving : you tap Save"]
        );
    }

    const PAY_STORY: &str = "## 1. Paying, charged twice

Rule broken: `NoDoubleCharge`
Why: each charge request charges again.
This is the only way to get here in the model.

0. Start: Cart
1. You pay. Now: Paying
2. The bank charges you. Now: Paying, charged
3. The bank charges you. Now: Paying, charged twice
";

    #[test]
    fn a_state_reached_only_after_a_bug_takes_the_bugs_color() {
        let spec = Spec::new("Pay", &pay("Pay", CHARGE_AGAIN, false), PAY_CFG, PAY_MAP);

        let drawing = spec.chart(&ChartOptions::default());

        let expected = [
            ("Cart", "works"),
            ("Paying", "works"),
            ("Paying, charged", "works"),
            ("Paying, charged twice", "wrong"),
            ("Refunded, charged", "wrong"),
        ]
        .map(|(state, class)| (state.to_owned(), class.to_owned()));
        assert_eq!(status_of(&drawing.mermaid), BTreeMap::from(expected));
    }

    #[test]
    fn each_bug_is_drawn_in_every_view_the_mapping_has() {
        let map = format!("{PAY_MAP}\nfn detail(v) {{ `charges = ${{v.charges}}` }}");
        let spec = Spec::new("Pay", &pay("Pay", CHARGE_AGAIN, true), PAY_CFG, &map);

        let charts = spec.draw();

        let files: Vec<&str> = charts.bugs.iter().map(|bug| bug.file.as_str()).collect();
        assert_eq!(
            files,
            ["bug-1.mmd", "bug-1-developer.mmd", "bug-1-layered.mmd"]
        );
        let layered = Regex::new(
            r#"state "Paying, charged twice" as g\d \{\n {4}state "charges = 2" as s\d\n"#,
        )
        .expect("a pattern");
        assert!(
            layered.is_match(&charts.bugs[2].mermaid),
            "{}",
            charts.bugs[2].mermaid
        );
        assert_eq!(charts.stories, PAY_STORY);
        assert_eq!(charts.summary["chartStates"], 5);
        assert_eq!(charts.summary["chartTransitions"], 4);
        assert_eq!(charts.summary["modelStates"], 5);
        assert_eq!(
            charts.summary["chartStatus"],
            json!({"works": 3, "wrong": 2, "fixed": 0})
        );
    }

    #[test]
    fn a_rule_broken_by_a_start_state_marks_that_state_wrong() {
        let spec = Spec::new(
            "Start",
            r#"---- MODULE Start ----
VARIABLES conn, badge
vars == <<conn, badge>>
Init == conn \in {"Online", "Offline"} /\ badge = "Synced"
Drop == conn = "Online" /\ conn' = "Offline" /\ badge' = "Waiting"
Next == Drop
Spec == Init /\ [][Next]_vars
NeverSyncedWhileOffline == ~(conn = "Offline" /\ badge = "Synced")
===="#,
            "SPECIFICATION Spec\nINVARIANT NeverSyncedWhileOffline\n",
            r#"
            fn state(v) { `${v.conn} | ${v.badge}` }
            fn step(a) { "the network drops" }
            "#,
        );

        let summary = spec.chart(&ChartOptions::default()).summary;

        let classes: Vec<(&Json, &Json)> = summary["classes"]
            .as_array()
            .expect("classes")
            .iter()
            .map(|class| (&class["end"], &class["readerSteps"]))
            .collect();
        assert_eq!(classes, [(&json!("Offline | Synced"), &json!(0))]);
        assert_eq!(
            summary["chartStatus"],
            json!({"works": 2, "wrong": 1, "fixed": 0})
        );
    }

    /// Each arrow as "from --> to : label", a box's arrow once per state inside it, and a state
    /// inside a box named "box: state".
    fn flat_edges(mermaid: &str) -> Vec<String> {
        let opens = Regex::new(r#"^ {2}state "([^"]+)" as (g\d+) \{$"#).expect("a pattern");
        let state = Regex::new(r#"^ +state "([^"]+)" as (s\d+)$"#).expect("a pattern");
        let arrow = Regex::new(r"(?m)^ {2}([gs]\d+) --> (s\d+) : (.*)$").expect("a pattern");
        let said = |text: &str| text.replace(BREAK, " ");
        let mut names: HashMap<String, String> = HashMap::new();
        let mut members: HashMap<String, Vec<String>> = HashMap::new();
        let mut open: Option<(String, String)> = None;
        for line in mermaid.lines() {
            if let Some(c) = opens.captures(line) {
                members.insert(c[2].to_owned(), Vec::new());
                open = Some((c[2].to_owned(), said(&c[1])));
            } else if line == "  }" {
                open = None;
            } else if let Some(c) = state.captures(line) {
                let name = match &open {
                    Some((id, box_name)) => {
                        members.entry(id.clone()).or_default().push(c[2].to_owned());
                        format!("{box_name}: {}", said(&c[1]))
                    }
                    None => said(&c[1]),
                };
                names.insert(c[2].to_owned(), name);
            }
        }
        let mut edges: Vec<String> = arrow
            .captures_iter(mermaid)
            .flat_map(|c| {
                let from = members
                    .get(&c[1])
                    .cloned()
                    .unwrap_or_else(|| vec![c[1].to_owned()]);
                from.iter()
                    .map(|f| format!("{} --> {} : {}", names[f], names[&c[2]], &c[3]))
                    .collect::<Vec<_>>()
            })
            .collect();
        edges.sort();
        edges
    }

    #[test]
    fn states_of_one_context_sit_in_one_box_and_the_box_border_stands_for_each() {
        let spec = Spec::new(
            "Window",
            r#"---- MODULE Window ----
VARIABLES where, reply
vars == <<where, reply>>
Init == where = "Starting" /\ reply = "none"
Boot == where = "Starting" /\ where' = "Chat open" /\ UNCHANGED reply
Ask == where = "Chat open" /\ reply = "none" /\ reply' = "pending" /\ UNCHANGED where
Answer == reply = "pending" /\ reply' = "shown" /\ UNCHANGED where
Close == where = "Chat open" /\ where' = "Window closed" /\ UNCHANGED reply
Open == where = "Window closed" /\ where' = "Chat open" /\ reply' = "none"
Next == Boot \/ Ask \/ Answer \/ Close \/ Open
Spec == Init /\ [][Next]_vars
NoReplyWhileClosed == ~(where = "Window closed" /\ reply = "shown")
===="#,
            "SPECIFICATION Spec\nINVARIANT NoReplyWhileClosed\n",
            r#"
            fn state(v) {
                let replies = #{ none: "no question", pending: "reply on its way", shown: "reply shown" };
                let place = v["where"];
                `${place}: ${if place == "Starting" { "loading" } else { replies[v.reply] }}`
            }
            fn step(a) {
                #{ Boot: "the app starts", Ask: "you ask", Answer: "the reply arrives", Close: "you close the window", Open: "you open the window" }[a]
            }
            "#,
        );
        let opening = |mermaid: &str| -> Vec<String> {
            mermaid
                .lines()
                .filter(|line| line.contains("you open the window"))
                .map(str::to_owned)
                .collect()
        };

        let boxed = spec
            .chart(&ChartOptions {
                highlight: 1,
                ..ChartOptions::default()
            })
            .mermaid;
        let flat = spec
            .chart(&ChartOptions {
                highlight: 1,
                flat: true,
                ..ChartOptions::default()
            })
            .mermaid;

        assert!(
            boxed.contains(concat!(
                "\n  state \"Chat open\" as g0 {\n    state \"no question\" as s0\n",
                "    state \"reply on its way\" as s1\n    state \"reply shown\" as s2\n  }\n",
                "  state \"Window closed\" as g1 {\n    state \"no question\" as s3\n",
                "    state \"reply on its way\" as s4\n    state \"reply shown\" as s5\n  }\n",
                "  state \"Starting: loading\" as s6\n",
            )),
            "{boxed}"
        );
        assert_eq!(opening(&boxed), ["  g1 --> s0 : you open the window"]);
        assert_eq!(opening(&flat).len(), 3);
        assert!(
            !Regex::new(r"as g\d+ \{")
                .expect("a pattern")
                .is_match(&flat)
        );
        assert_eq!(flat_edges(&boxed), flat_edges(&flat));
        assert!(boxed.lines().any(|line| line == "  class s5 wrongPath"));
        assert!(
            Regex::new(r"(?m)^ {2}s\d --> s5 : 4\. ")
                .expect("a pattern")
                .is_match(&boxed)
        );
        assert!(boxed.contains("state \"Breaks a rule\" as legendWrong"));
    }

    const DOOR_SPEC: &str = r#"---- MODULE Door ----
VARIABLES where, reply
vars == <<where, reply>>
Init == where = "Open" /\ reply = "none"
Ask == reply = "none" /\ reply' = "pending" /\ UNCHANGED where
Answer == where = "Open" /\ reply = "pending" /\ reply' = "shown" /\ UNCHANGED where
Drop == where = "Closed" /\ reply = "pending" /\ reply' = "lost" /\ UNCHANGED where
Close == where = "Open" /\ where' = "Closed" /\ UNCHANGED reply
Reopen == where = "Closed" /\ where' = "Open" /\ (reply' = reply \/ (reply = "pending" /\ reply' = "shown"))
Next == Ask \/ Answer \/ Drop \/ Close \/ Reopen
Spec == Init /\ [][Next]_vars
NoLostReply == reply # "lost"
NoShownWhileClosed == ~(where = "Closed" /\ reply = "shown")
===="#;
    const DOOR_MAP: &str = r#"
        fn region_names() { ["Where you are", "Your reply"] }
        fn state(v) {
            let places = #{ Open: "Window open", Closed: "Window closed" };
            let replies = #{ none: "no question", pending: "reply on its way", shown: "reply shown", lost: "reply lost" };
            `${places[v["where"]]}: ${replies[v.reply]}`
        }
        fn step(a) {
            #{ Ask: "you ask", Answer: "the reply arrives", Drop: "the app drops the reply", Close: "you close the window", Reopen: "you open the window" }[a]
        }
    "#;
    const DOOR_GUARD: &str = r#"
        fn guard(a, before) {
            if a == "Answer" { "window open" }
            else if a == "Drop" { "window closed" }
            else if a == "Ask" && before["where"] == "Open" { "window open" }
            else { "" }
        }
    "#;
    const DOOR_TIMER: &str = r#"fn timer(a) { if a == "Drop" { 30 } }"#;
    const DOOR_GROUP: &str = r##"
        fn group(v) {
            #{ none: "Nothing to show", pending: "Waiting", shown: "Reply shown", lost: "Reply shown" }[v.reply]
        }
        fn group_colors() { [["Reply shown", "#d1f2eb"], ["Waiting", ()], ["Never drawn", "#ffffff"]] }
    "##;
    const DOOR_MAP_NAME: &str = "Door.map.rhai";
    const NO_LOST_REPLY: &str = "NoLostReply";
    const NO_SHOWN_WHILE_CLOSED: &str = "NoShownWhileClosed";

    struct Door {
        charts: Charts,
        chart: Vec<String>,
        every: Option<Vec<String>>,
        bug: String,
    }

    /// Upstream's `runDoor`: one from-spec run that checks `rule` alone.
    fn door(extra: &str, rule: &str) -> Door {
        let charts = Spec::new(
            "Door",
            DOOR_SPEC,
            &rule_cfg(rule),
            &format!("{DOOR_MAP}{extra}"),
        )
        .draw();
        Door {
            chart: unwrapped_lines(&charts.chart),
            every: charts.regions_all.as_deref().map(unwrapped_lines),
            bug: charts.bugs[0].mermaid.replace(BREAK, " "),
            charts,
        }
    }

    impl Door {
        fn line_after(&self, words: &str) -> &str {
            let at = self
                .chart
                .iter()
                .position(|line| line.ends_with(words))
                .unwrap_or_else(|| panic!("an arrow says {words}"));
            &self.chart[at + 1]
        }

        fn proves_its_regions(&self) {
            let all = parse(self.charts.regions_all.as_deref().expect("all arrows")).expect("all");
            let proof =
                parse(self.charts.regions_proof.as_deref().expect("a proof")).expect("the proof");
            assert_eq!(compare(&all.regions, &proof.regions), []);
        }
    }

    fn matches_any(lines: &[String], pattern: &str) -> usize {
        let pattern = Regex::new(pattern).expect("a pattern");
        lines.iter().filter(|line| pattern.is_match(line)).count()
    }

    #[test]
    fn parallel_regions_draw_only_when_they_combine_back_into_the_graph() {
        let proved = door("", NO_LOST_REPLY);

        assert_eq!(proved.charts.summary["chartStates"], 6);
        assert_eq!(proved.charts.summary["chartTransitions"], 5);
        assert_eq!(proved.charts.summary["modelStates"], 8);
        assert_eq!(
            proved.charts.summary["chartStatus"],
            json!({"works": 5, "wrong": 1, "fixed": 0})
        );
        assert_eq!(
            proved.charts.summary["regions"],
            json!({"drawn": true, "states": [2, 4], "transitions": 5, "all": 7, "flat": {"states": 8, "transitions": 13}})
        );
        assert!(
            proved
                .chart
                .iter()
                .any(|l| l == "    state \"Where you are\" as region0 {")
        );
        assert!(proved.chart.iter().any(|l| l == "    --"));
        assert_eq!(
            matches_any(
                &[proved
                    .line_after(": the reply arrives [when Window open]")
                    .to_owned()],
                r"^ {6}%% when r0s\d$"
            ),
            1
        );
        assert_eq!(
            matches_any(&proved.chart, r"^ {6}r1s\d --> r1s\d : you open the window"),
            0
        );
        let every = proved.every.as_deref().expect("all arrows");
        let reopens =
            Regex::new(r"^ {6}(r1s\d) --> (r1s\d) : you open the window \[when Window closed\]$")
                .expect("a pattern");
        assert_eq!(
            every
                .iter()
                .filter_map(|line| reopens.captures(line))
                .filter(|c| c[1] == c[2])
                .count(),
            1
        );
        proved.proves_its_regions();

        let boxed = door("", NO_SHOWN_WHILE_CLOSED);

        assert_eq!(boxed.every, None);
        assert_eq!(
            boxed.charts.summary["regions"]["why"],
            "\"Window closed: reply shown\" is \"Breaks a rule\", and its two region states would show \"Works\""
        );
        assert_eq!(matches_any(&boxed.chart, r"^ +--$"), 0);
        assert_eq!(
            matches_any(&boxed.chart, r#"^ {2}state "Window closed" as g\d \{$"#),
            1
        );
    }

    #[test]
    fn guard_words_show_on_an_arrow_only_when_every_step_behind_it_has_them() {
        let proved = door(DOOR_GUARD, NO_LOST_REPLY);

        for words in [
            ": the reply arrives [window open]",
            ": the app drops the reply [window closed]",
        ] {
            assert_eq!(
                matches_any(
                    &[proved.line_after(words).to_owned()],
                    r"^ {6}%% when r0s\d$"
                ),
                1,
                "{words}"
            );
        }
        assert_eq!(
            matches_any(&proved.chart, r"^ {6}r1s\d --> r1s\d : you ask$"),
            1
        );
        proved.proves_its_regions();
        assert!(
            Regex::new(r"(?m) : \d\. the app drops the reply \[window closed\]$")
                .expect("a pattern")
                .is_match(&proved.bug),
            "{}",
            proved.bug
        );

        let boxed = door(DOOR_GUARD, NO_SHOWN_WHILE_CLOSED);

        assert!(
            boxed
                .chart
                .iter()
                .any(|l| l.ends_with(" : you ask [window open]"))
        );
        assert!(boxed.chart.iter().any(|l| l.ends_with(" : you ask")));
    }

    #[test]
    fn a_timer_step_says_after_n_seconds() {
        let timed = door(DOOR_TIMER, NO_LOST_REPLY);

        assert!(
            timed
                .chart
                .iter()
                .any(|l| l.ends_with(" : after 30 s, the app drops the reply [when Window closed]"))
        );
        assert!(
            Regex::new(r"(?m) : \d\. after 30 s, the app drops the reply$")
                .expect("a pattern")
                .is_match(&timed.bug),
            "{}",
            timed.bug
        );
        timed.proves_its_regions();
    }

    #[test]
    fn a_group_fills_its_states_and_the_legend_lists_only_drawn_groups() {
        let run = door(DOOR_GROUP, NO_LOST_REPLY);

        let head = run
            .chart
            .iter()
            .position(|l| l == "  state \"At the same time\" as regions {")
            .expect("the regions");
        assert_eq!(
            run.chart[..head],
            [
                "---",
                "config:",
                "  markdownAutoWrap: false",
                "  state:",
                "    useMaxWidth: false",
                "---",
                "stateDiagram-v2",
                "  classDef plain fill:#f4f6f7,stroke:#566573,color:#1b2631",
                "  classDef plainWrong fill:#f4f6f7,stroke:#c0392b,stroke-width:4px,color:#7b241c",
                "  classDef group0 fill:#d1f2eb,stroke:#697976,color:#1b2631",
                "  classDef group0Wrong fill:#d1f2eb,stroke:#c0392b,stroke-width:4px,color:#7b241c",
                "  classDef group1 fill:#d6eaf8,stroke:#6b757c,color:#1b2631",
                "  classDef group2 fill:#fdebd0,stroke:#7f7668,color:#1b2631",
                "  state \"Legend\" as legend {",
                "    state \"Reply shown\" as legendGroup0",
                "    state \"Waiting\" as legendGroup1",
                "    state \"Nothing to show\" as legendGroup2",
                "    state \"Breaks a rule\" as legendWrong",
                "  }",
                "  class legendGroup0 group0",
                "  class legendGroup1 group1",
                "  class legendGroup2 group2",
                "  class legendWrong plainWrong",
            ]
        );
        assert!(
            run.chart
                .iter()
                .any(|l| l == "      state \"reply lost\" as r1s1")
        );
        let classes: Vec<&str> = run
            .chart
            .iter()
            .filter(|l| l.starts_with("  class r"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            classes,
            [
                "  class r0s0 plain",
                "  class r0s1 plain",
                "  class r1s0 group2",
                "  class r1s1 group0Wrong",
                "  class r1s2 group1",
                "  class r1s3 group0",
            ]
        );
        assert_eq!(
            run.charts.summary["chartStatus"],
            json!({"works": 5, "wrong": 1, "fixed": 0})
        );
        run.proves_its_regions();
        let bug: Vec<&str> = run.bug.lines().collect();
        assert!(bug.contains(
            &"  classDef group1Path fill:#d6eaf8,stroke:#1f3a93,stroke-width:4px,color:#1b2631"
        ));
        let bug_classes: Vec<&str> = bug
            .iter()
            .copied()
            .filter(|l| {
                Regex::new(r"^ {2}class s\d")
                    .expect("a pattern")
                    .is_match(l)
            })
            .collect();
        assert_eq!(
            bug_classes,
            [
                "  class s0 group0Wrong",
                "  class s1 group1Path",
                "  class s2 group2Path",
                "  class s3 group1Path",
            ]
        );
        assert_eq!(
            run.charts.summary["chartGroups"],
            json!([
                {"name": "Reply shown", "color": "#d1f2eb", "states": 2},
                {"name": "Waiting", "color": "#d6eaf8", "states": 1},
                {"name": "Nothing to show", "color": "#fdebd0", "states": 1},
            ])
        );
        assert_eq!(run.charts.summary["mixedGroups"], json!([]));
    }

    fn door_spec(map: &str) -> Spec {
        Spec::new("Door", DOOR_SPEC, &rule_cfg(NO_LOST_REPLY), map)
    }

    #[test]
    fn a_group_cannot_hide_a_broken_rule() {
        let hidden = door_spec(&format!(
            r##"{DOOR_MAP}
            fn group(v) {{ "All good" }}
            fn group_colors() {{ [["All good", "#e9f7ef"]] }}
            "##
        ))
        .chart(&ChartOptions {
            regions: true,
            ..ChartOptions::default()
        });

        let lines: Vec<&str> = hidden.mermaid.lines().collect();
        assert!(lines.contains(
            &"  classDef group0Wrong fill:#e9f7ef,stroke:#c0392b,stroke-width:4px,color:#7b241c"
        ));
        let legend = lines
            .iter()
            .position(|l| *l == "  state \"Legend\" as legend {")
            .expect("a legend");
        assert_eq!(
            lines[legend..legend + 6],
            [
                "  state \"Legend\" as legend {",
                "    state \"All good\" as legendGroup0",
                "    state \"Breaks a rule\" as legendWrong",
                "  }",
                "  class legendGroup0 group0",
                "  class legendWrong plainWrong",
            ]
        );
        assert!(lines.contains(&"  class r1s1 group0Wrong"));
        assert_eq!(
            lines.iter().filter(|l| l.ends_with(" group0Wrong")).count(),
            1
        );
    }

    #[test]
    fn a_group_wider_than_a_state_counts_as_mixed() {
        let coarse = door_spec(
            r#"
            fn state(v) { #{ Open: "Window open", Closed: "Window closed" }[v["where"]] }
            fn group(v) { if v.reply == "shown" { "Reply shown" } else { "Waiting" } }
            "#,
        )
        .chart(&ChartOptions::default());

        let classes: Vec<&str> = coarse
            .mermaid
            .lines()
            .filter(|l| l.starts_with("  class ") || l.starts_with("  classDef "))
            .collect();
        assert_eq!(
            classes,
            [
                "  classDef plain fill:#f4f6f7,stroke:#566573,color:#1b2631",
                "  classDef plainWrong fill:#f4f6f7,stroke:#c0392b,stroke-width:4px,color:#7b241c",
                "  class legendWrong plainWrong",
                "  class s0 plainWrong",
                "  class s1 plain",
            ]
        );
        assert_eq!(coarse.summary["chartGroups"], json!([]));
        assert_eq!(
            coarse.summary["mixedGroups"],
            json!(["Window closed", "Window open"])
        );
    }

    #[test_case(r#"fn group(v) { if v.reply == "lost" { "Breaks a rule" } else { "Fine" } }"# => matches ChartError::StatusGroup(group) if group == "Breaks a rule"; "the wrong legend")]
    #[test_case(r#"fn group(v) { "fixed" }"# => matches ChartError::StatusGroup(group) if group == "fixed"; "a status name")]
    #[test_case(r##"fn group(v) { "All good" } fn group_colors() { [["All good", "#123"]] }"## => matches ChartError::GroupColor { color, .. } if color == "#123"; "a dark color")]
    #[test_case(r#"fn group(v) { "All good" } fn group_colors() { [["All good", "red"]] }"# => matches ChartError::GroupColor { color, .. } if color == "red"; "not a hex color")]
    #[test_case(r##"fn group(v) { "All good" } fn group_colors() { ["#ffffff"] }"## => matches ChartError::Mapping(_); "colors that are not pairs")]
    #[test_case("fn group(v) { 1 }" => matches ChartError::Mapping(MappingError::Returned { function: "group", got, .. }) if got == "1"; "a group that is not a name")]
    fn refused_groups(group: &str) -> ChartError {
        let mapping = match Mapping::parse(DOOR_MAP_NAME, &format!("{DOOR_MAP}{group}")) {
            Ok(mapping) => mapping,
            Err(error) => return ChartError::Mapping(error),
        };
        let model = explore_text("Door", DOOR_SPEC, &rule_cfg(NO_LOST_REPLY));
        let rules = HashMap::new();
        Charter::new(&model, &mapping, &rules)
            .and_then(|charter| {
                charter.chart(&ChartOptions {
                    regions: true,
                    ..ChartOptions::default()
                })
            })
            .expect_err("the mapping is refused")
    }

    #[test]
    fn each_looping_step_keeps_its_label_and_a_null_cause_groups_bugs_by_end_state() {
        let spec = Spec::new(
            "Browse",
            r#"---- MODULE Browse ----
EXTENDS Naturals
VARIABLES screen, looks, scrolled
vars == <<screen, looks, scrolled>>
Init == screen = "Cart" /\ looks = 0 /\ scrolled = FALSE
Look == screen = "Cart" /\ looks < 3 /\ looks' = looks + 1 /\ UNCHANGED <<screen, scrolled>>
Scroll == screen = "Cart" /\ looks = 1 /\ ~scrolled /\ scrolled' = TRUE /\ UNCHANGED <<screen, looks>>
Pay == screen = "Cart" /\ looks = 3 /\ scrolled /\ screen' = "Paying" /\ UNCHANGED <<looks, scrolled>>
Fail == screen = "Paying" /\ screen' = "Failed" /\ UNCHANGED <<looks, scrolled>>
Hang == screen = "Paying" /\ screen' = "Stuck" /\ UNCHANGED <<looks, scrolled>>
Next == Look \/ Scroll \/ Pay \/ Fail \/ Hang
Spec == Init /\ [][Next]_vars
NeverLost == screen \notin {"Failed", "Stuck"}
===="#,
            "SPECIFICATION Spec\nINVARIANT NeverLost\n",
            r#"
            fn reader_actions() { ["Look", "Scroll", "Pay"] }
            fn state(v) { v.screen }
            fn step(a) { #{ Look: "you look", Scroll: "you scroll", Pay: "you pay", Fail: "the payment fails", Hang: "the payment hangs" }[a] }
            fn cause() { () }
            fn legend() { #{ works: "Works on main", wrong: "Wrong on main" } }
            "#,
        );
        let loops = |mermaid: &str| -> Vec<String> {
            let arrow = Regex::new(r"(?m)^ {2}(s\d+) --> (s\d+) : (.*)$").expect("a pattern");
            arrow
                .captures_iter(mermaid)
                .filter(|c| c[1] == c[2])
                .map(|c| c[3].to_owned())
                .collect()
        };

        let charts = spec.draw();

        let classes: Vec<(&Json, &Json)> = charts.summary["classes"]
            .as_array()
            .expect("classes")
            .iter()
            .map(|class| (&class["end"], &class["cause"]))
            .collect();
        assert_eq!(
            classes,
            [
                (&json!("Failed"), &Json::Null),
                (&json!("Stuck"), &Json::Null)
            ]
        );
        assert!(
            !Regex::new(r"(?m)^Why:|null")
                .expect("a pattern")
                .is_match(&charts.stories)
        );
        assert_eq!(
            loops(&charts.bugs[0].mermaid),
            ["1. you look<br/>2. you scroll<br/>3, 4. you look"]
        );
        assert_eq!(loops(&charts.chart), ["you look or you scroll"]);
        assert!(
            charts
                .chart
                .contains("state \"Works on main\" as legendWorks")
        );
        assert!(
            charts
                .chart
                .contains("state \"Wrong on main\" as legendWrong")
        );
        assert!(!charts.chart.contains("legendFixed"));
    }
}
