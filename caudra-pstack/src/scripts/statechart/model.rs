//! The model a chart is drawn from: every state the spec reaches, every step between them, and
//! the shortest path to each state that first breaks a rule. Upstream reads these from TLC's
//! `-dump dot,actionlabels` graph and from one `-continue` run per rule, whose `CONSTRAINT` on the
//! rule stops each path at its first bad state. tla-checker explores once, in process, and the
//! paths come from a breadth-first search of its graph that stops the same way.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::iter;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value as Json};
use thiserror::Error;
use tla_checker::ast::{State, Value};
use tla_checker::checker::{
    CheckResult, CheckerConfig, ExploredGraph, LivenessError, PrepareSpecError, check,
    eval_error_to_diagnostic,
};
use tla_checker::config::parse_cfg;
use tla_checker::diagnostic::Diagnostic;
use tla_checker::load::{PrepareError, Prepared, prepare_from_path};
use tla_checker::source::Source;
use tla_checker::span::Span;

/// Past this many states the search stops rather than exhaust memory: TLC keeps its states on
/// disk, tla-checker in memory.
pub const MAX_STATES: usize = 5_000_000;

/// One state's variables, valued as upstream's `parseValue` reads TLC's output: sequences, tuples
/// and sets become arrays, records and functions become objects, and model values become strings.
pub type Vars = Map<String, Json>;
/// A behavior's states, each with the action that reached it.
pub type Behavior = Vec<(Option<Arc<str>>, Vars)>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Step {
    pub from: usize,
    pub to: usize,
    pub action: Option<Arc<str>>,
}

/// The shortest path to a state that breaks `rule`, through states that keep it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub rule: Arc<str>,
    pub states: Vec<usize>,
    /// The action that takes each step: one fewer than `states`.
    pub actions: Vec<Option<Arc<str>>>,
}

#[derive(Debug)]
pub struct Model {
    pub states: Vec<Vars>,
    pub initial: Vec<usize>,
    /// Each step once, in the order the search took them, self-loops included.
    pub steps: Vec<Step>,
    /// The cfg's `INVARIANT` rules, in its order.
    pub rules: Vec<Arc<str>>,
    /// Per rule in `rules` order, one trace per bad state, in the order the search reached them.
    pub traces: Vec<Trace>,
}

impl Model {
    pub fn broken(&self, rule: &str) -> bool {
        self.traces.iter().any(|trace| trace.rule.as_ref() == rule)
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("{0}")]
    Spec(String),
    #[error(
        "the spec reaches more than {MAX_STATES} states. Bound the model: fewer runs, shorter queues, smaller sets of values."
    )]
    TooManyStates,
    #[error("no state satisfies Init")]
    NoInitialStates,
    #[error(
        "{cfg}: ACTION_CONSTRAINT {names} is not supported, so every step it forbids would be drawn. Fold the condition into the actions it constrains, or into Next."
    )]
    ActionConstraint { cfg: String, names: String },
    #[error("tla-checker stopped unexpectedly: {0}")]
    Unexpected(String),
}

/// Explores `spec` under `cfg`. The cfg's `PROPERTY` lines are not checked here: the chart draws
/// only `INVARIANT` rules.
pub fn explore(spec: &Path, cfg: &Path) -> Result<Model, ModelError> {
    let mut prepared = prepare(spec, cfg)?;
    refuse_action_constraints(cfg)?;
    prepared.spec.liveness_properties.clear();
    prepared.spec.safety_properties.clear();
    let config = CheckerConfig {
        keep_graph: true,
        continue_on_violation: true,
        allow_deadlock: true,
        check_liveness: false,
        quiet: true,
        max_states: MAX_STATES,
        max_depth: usize::MAX,
        ..prepared.checker_config
    };
    let graph = match check(&prepared.spec, &prepared.domains, &config) {
        CheckResult::Ok(stats) => stats
            .graph
            .ok_or_else(|| ModelError::Unexpected("no graph".into()))?,
        CheckResult::MaxStatesExceeded(_) => return Err(ModelError::TooManyStates),
        CheckResult::NoInitialStates => return Err(ModelError::NoInitialStates),
        CheckResult::InitError(error)
        | CheckResult::NextError(error, ..)
        | CheckResult::InvariantError(error, ..) => {
            return Err(ModelError::Spec(
                eval_error_to_diagnostic(&error).render(&prepared.source),
            ));
        }
        CheckResult::PrepareError(error) => {
            return Err(ModelError::Spec(prepare_error(error, &prepared.source)));
        }
        other => return Err(ModelError::Unexpected(format!("{other:?}"))),
    };
    let rules: Vec<Arc<str>> = prepared
        .spec
        .invariant_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            name.clone()
                .unwrap_or_else(|| format!("INVARIANT {}", index + 1).into())
        })
        .collect();
    Ok(Model {
        states: graph
            .states
            .iter()
            .map(|state| vars(&prepared.spec.vars, state))
            .collect(),
        initial: graph
            .parents
            .iter()
            .enumerate()
            .filter_map(|(index, parent)| parent.is_none().then_some(index))
            .collect(),
        steps: steps(&graph),
        traces: rules
            .iter()
            .enumerate()
            .flat_map(|(index, rule)| traces(&graph, index, rule))
            .collect(),
        rules,
    })
}

/// Checks the `PROPERTY` lines of `cfg` on `spec`, as upstream's one TLC run per property does.
/// `None` when they hold; otherwise the behavior that breaks one, each state with the action
/// that reached it.
pub fn property_counterexample(spec: &Path, cfg: &Path) -> Result<Option<Behavior>, ModelError> {
    let prepared = prepare(spec, cfg)?;
    let config = CheckerConfig {
        allow_deadlock: true,
        check_liveness: true,
        quiet: true,
        max_states: MAX_STATES,
        max_depth: usize::MAX,
        ..prepared.checker_config
    };
    let named = |states: Vec<State>, actions: Vec<Option<Arc<str>>>| {
        iter::once(None)
            .chain(actions)
            .chain(iter::repeat(None))
            .zip(states.iter().map(|state| vars(&prepared.spec.vars, state)))
            .collect()
    };
    match check(&prepared.spec, &prepared.domains, &config) {
        CheckResult::Ok(_) => Ok(None),
        CheckResult::PropertyViolation(violation, _) => {
            Ok(Some(named(violation.trace, violation.actions)))
        }
        CheckResult::InvariantViolation(counterexample, _) => {
            Ok(Some(named(counterexample.trace, counterexample.actions)))
        }
        CheckResult::LivenessViolation(violation, _) => {
            let states = [violation.prefix, violation.cycle].concat();
            Ok(Some(named(states, Vec::new())))
        }
        CheckResult::MaxStatesExceeded(_) => Err(ModelError::TooManyStates),
        CheckResult::NoInitialStates => Err(ModelError::NoInitialStates),
        CheckResult::InitError(error)
        | CheckResult::NextError(error, ..)
        | CheckResult::InvariantError(error, ..)
        | CheckResult::LivenessError(LivenessError { error, .. }, _) => Err(ModelError::Spec(
            eval_error_to_diagnostic(&error).render(&prepared.source),
        )),
        CheckResult::PrepareError(error) => {
            Err(ModelError::Spec(prepare_error(error, &prepared.source)))
        }
        other => Err(ModelError::Unexpected(format!("{other:?}"))),
    }
}

fn prepare(spec: &Path, cfg: &Path) -> Result<Prepared, ModelError> {
    prepare_from_path(spec, Some(cfg), &[]).map_err(|error| match error {
        PrepareError::Parse { message, span } => ModelError::Spec(parse_error(spec, message, span)),
        other => ModelError::Spec(format!("{}: {other}", spec.display())),
    })
}

/// tla-checker only warns about `ACTION_CONSTRAINT`, and then explores the steps it forbids. The
/// cfg already parsed once, so a second parse cannot fail.
fn refuse_action_constraints(cfg: &Path) -> Result<(), ModelError> {
    let text = fs::read_to_string(cfg).unwrap_or_default();
    let Ok(parsed) = parse_cfg(&text) else {
        return Ok(());
    };
    if parsed.action_constraints.is_empty() {
        return Ok(());
    }
    Err(ModelError::ActionConstraint {
        cfg: cfg.display().to_string(),
        names: parsed.action_constraints.join(", "),
    })
}

fn steps(graph: &ExploredGraph) -> Vec<Step> {
    let mut seen = HashSet::new();
    graph
        .edges
        .iter()
        .enumerate()
        .flat_map(|(from, out)| {
            out.iter().map(move |(to, action)| Step {
                from,
                to: *to,
                action: action.clone(),
            })
        })
        .filter(|step| seen.insert(step.clone()))
        .collect()
}

/// A breadth-first search from the initial states that does not go past a state breaking the
/// rule, as TLC's run with that rule as its `CONSTRAINT` does. Each bad state is reached first by
/// a shortest path, and the edge order decides between equally short ones.
fn traces(graph: &ExploredGraph, rule_index: usize, rule: &Arc<str>) -> Vec<Trace> {
    let bad: HashSet<usize> = graph
        .violations
        .iter()
        .filter(|(_, rule)| *rule == rule_index)
        .map(|(state, _)| *state)
        .collect();
    if bad.is_empty() {
        return Vec::new();
    }
    let count = graph.states.len();
    let mut reached_from: Vec<Option<(usize, Option<Arc<str>>)>> = vec![None; count];
    let mut seen = vec![false; count];
    let mut queue = VecDeque::new();
    let mut found = Vec::new();
    let mut visit = |state: usize, queue: &mut VecDeque<usize>| {
        if bad.contains(&state) {
            found.push(state);
        } else {
            queue.push_back(state);
        }
    };
    for (state, parent) in graph.parents.iter().enumerate() {
        if parent.is_none() {
            seen[state] = true;
            visit(state, &mut queue);
        }
    }
    while let Some(from) = queue.pop_front() {
        for (to, action) in &graph.edges[from] {
            if !seen[*to] {
                seen[*to] = true;
                reached_from[*to] = Some((from, action.clone()));
                visit(*to, &mut queue);
            }
        }
    }
    found
        .into_iter()
        .map(|end| {
            let mut states = vec![end];
            let mut actions = Vec::new();
            while let Some((from, action)) = &reached_from[states[states.len() - 1]] {
                states.push(*from);
                actions.push(action.clone());
            }
            states.reverse();
            actions.reverse();
            Trace {
                rule: rule.clone(),
                states,
                actions,
            }
        })
        .collect()
}

fn vars(names: &[Arc<str>], state: &State) -> Vars {
    names
        .iter()
        .zip(&state.values)
        .map(|(name, value)| (name.to_string(), json(value)))
        .collect()
}

fn json(value: &Value) -> Json {
    match value {
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Str(text) | Value::Model(text) => Json::String(text.to_string()),
        Value::Set(items) => Json::Array(items.iter().map(json).collect()),
        Value::Tuple(items) => Json::Array(items.iter().map(json).collect()),
        Value::Record(fields) => Json::Object(
            fields
                .iter()
                .map(|(field, value)| (field.to_string(), json(value)))
                .collect(),
        ),
        Value::Fn(pairs) => Json::Object(
            pairs
                .iter()
                .map(|(key, value)| (key_text(key), json(value)))
                .collect(),
        ),
        Value::IntSet(domain) => Json::String(domain.name().to_string()),
    }
}

/// A function's key as upstream names it: a string as itself, anything else as its JSON.
fn key_text(key: &Value) -> String {
    match json(key) {
        Json::String(text) => text,
        other => other.to_string(),
    }
}

fn parse_error(spec: &Path, message: String, span: Option<Span>) -> String {
    let diagnostic = Diagnostic::error(message);
    match (span, fs::read_to_string(spec)) {
        (Some(span), Ok(text)) => diagnostic
            .with_span(span)
            .render(&Source::new(spec.display().to_string(), text)),
        _ => diagnostic.render_simple(),
    }
}

fn prepare_error(error: PrepareSpecError, source: &Source) -> String {
    let diagnostic = match error {
        PrepareSpecError::InstanceError(error) | PrepareSpecError::AssumeError(_, error) => {
            eval_error_to_diagnostic(&error)
        }
        PrepareSpecError::MissingConstants(names) => Diagnostic::error(format!(
            "the cfg gives no value to {}",
            names
                .iter()
                .map(|name| name.as_ref())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        PrepareSpecError::AssumeViolation(index) => {
            Diagnostic::error(format!("ASSUME {} does not hold", index + 1))
        }
        PrepareSpecError::NonModelValueSymmetry(name, _) => Diagnostic::error(format!(
            "the symmetry set {name} holds values that are not model values"
        )),
        PrepareSpecError::RefinementConfigError(message)
        | PrepareSpecError::ModuleParse(message)
        | PrepareSpecError::LivenessProperty(message)
        | PrepareSpecError::ConstantSubstitution(message) => Diagnostic::error(message),
    };
    diagnostic.render(source)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statechart");
    const BAD_SPEC_FILE: &str = "Bad.tla:";
    const BAD_SPEC_LINE: &str = "line 4";

    fn explored(name: &str) -> Model {
        let dir = PathBuf::from(FIXTURES);
        explore(
            &dir.join(format!("{name}.tla")),
            &dir.join(format!("{name}.cfg")),
        )
        .expect("the fixture should explore")
    }

    fn actions(trace: &Trace) -> Vec<&str> {
        trace
            .actions
            .iter()
            .map(|action| action.as_deref().unwrap_or_default())
            .collect()
    }

    #[test_case("OutputFolder", 4294, 11824; "output folder matches tlc")]
    #[test_case("Sync", 4, 9; "sync matches tlc")]
    #[test_case("Upload", 5, 8; "upload matches tlc")]
    fn graph_has_tlcs_states_and_steps(name: &str, states: usize, steps: usize) {
        let model = explored(name);

        assert_eq!((model.states.len(), model.steps.len()), (states, steps));
    }

    #[test_case("OutputFolder"; "output folder")]
    #[test_case("Sync"; "sync")]
    #[test_case("Upload"; "upload, whose actions are disjunctions")]
    fn every_step_is_named(name: &str) {
        let model = explored(name);

        assert!(model.steps.iter().all(|step| step.action.is_some()));
    }

    #[test]
    fn sync_breaks_one_rule_by_tlcs_path() {
        let model = explored("Sync");

        assert_eq!(
            model.rules,
            [Arc::from("TypeOK"), Arc::from("NeverSyncedWhileOffline")]
        );
        assert!(!model.broken("TypeOK"));
        let [trace] = model.traces.as_slice() else {
            panic!("one bad state, got {:?}", model.traces);
        };
        assert_eq!(actions(trace), ["TheNetworkDrops", "TheServerConfirms"]);
        assert_eq!(
            model.states[trace.states[2]],
            *json!({"connection": "Offline", "syncBadge": "Synced"})
                .as_object()
                .unwrap()
        );
    }

    #[test_case("NoNotesAside", &["Start", "Tlc", "Tlc", "Draw", "Swap", "Note", "MoveAside"]; "no notes aside as tlc")]
    #[test_case("NeverExitTwo", &["Start", "Refuse"]; "never exit two as tlc")]
    #[test_case("NoBroke", &["Start", "Tlc", "Tlc", "Draw", "Swap", "RenameFails"]; "no broke as tlc")]
    #[test_case("NotBothWrote", &["Start", "Start", "Tlc", "Tlc", "Tlc", "Tlc", "Draw", "Draw", "Swap", "Swap", "MoveIn", "MoveAside", "Checked", "MoveIn"]; "not both wrote takes each action for every run where tlc takes every action of run one first")]
    fn first_trace_is_a_shortest_path(rule: &str, expected: &[&str]) {
        let model = explored("BrokenFolder");

        let first = model
            .traces
            .iter()
            .find(|trace| trace.rule.as_ref() == rule)
            .expect("the rule should break");
        assert_eq!(actions(first), expected);
    }

    #[test]
    fn traces_stop_at_the_first_bad_state() {
        let model = explored("BrokenFolder");

        for trace in &model.traces {
            let bad: Vec<_> = model
                .traces
                .iter()
                .filter(|other| other.rule == trace.rule)
                .map(|other| other.states[other.states.len() - 1])
                .collect();
            let (end, before) = trace.states.split_last().unwrap();
            assert!(bad.contains(end));
            assert!(before.iter().all(|state| !bad.contains(state)));
        }
    }

    #[test]
    fn values_read_as_upstream_parses_them() {
        let model = explored("OutputFolder");

        let folder = json!({"by": 0, "exists": false, "notes": false, "read": []});
        assert_eq!(
            model.states[model.initial[0]]["aside"],
            json!([folder.clone(), folder])
        );
        assert_eq!(model.states[model.initial[0]]["read"], json!([[], []]));
    }

    #[test]
    fn a_parse_error_names_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("Bad.tla");
        fs::write(
            &spec,
            "---- MODULE Bad ----\nVARIABLE x\nInit == x = \nNext == x' = x\n====\n",
        )
        .unwrap();
        let cfg = dir.path().join("Bad.cfg");
        fs::write(&cfg, "INIT Init\nNEXT Next\n").unwrap();

        let error = explore(&spec, &cfg).unwrap_err().to_string();

        assert!(error.contains(BAD_SPEC_FILE), "{error}");
        assert!(error.contains(BAD_SPEC_LINE), "{error}");
    }

    #[test]
    fn an_action_constraint_is_refused_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("Counter.tla");
        fs::write(
            &spec,
            "---- MODULE Counter ----\nVARIABLE x\nInit == x = 0\nNext == x' = (x + 1) % 3\nSmall == x' < 2\n====\n",
        )
        .unwrap();
        let cfg = dir.path().join("Counter.cfg");
        fs::write(&cfg, "INIT Init\nNEXT Next\nACTION_CONSTRAINT Small\n").unwrap();

        let error = explore(&spec, &cfg).unwrap_err();

        assert!(
            matches!(&error, ModelError::ActionConstraint { names, .. } if names == "Small"),
            "{error}"
        );
    }
}
