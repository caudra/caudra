//! The mapping that labels a model in the reader's words: `SPEC.map.rhai`, upstream's
//! `SPEC.map.mjs` in Rhai. It holds only functions, run in the shared script sandbox, and every
//! name upstream exports is a function here: `state`, `detail`, `step`, `guard`, `timer`, `cause`
//! and `group` take the values upstream passes them, and `reader_actions`, `region_names`,
//! `legend` and `group_colors` take none and return what upstream's constants hold.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_script::{SandboxLimits, restricted_engine};
use regex::Regex;
use rhai::serde::to_dynamic;
use rhai::{AST, Array, CallFnOptions, Dynamic, Engine, ImmutableString, Map as RhaiMap, Scope};
use serde_json::Value as Json;
use thiserror::Error;

use super::model::Vars;

const LIMITS: SandboxLimits = SandboxLimits {
    max_operations: 1_000_000,
    max_call_levels: 64,
    max_expr_depth: 128,
    max_string_size: 256 * 1024,
    max_array_size: 100_000,
    max_map_size: 100_000,
};
const HINT: &str = "a statechart mapping; return a value instead";

const STATE: &str = "state";
const DETAIL: &str = "detail";
const STEP: &str = "step";
const GUARD: &str = "guard";
const TIMER: &str = "timer";
const CAUSE: &str = "cause";
const GROUP: &str = "group";
const READER_ACTIONS: &str = "reader_actions";
const REGION_NAMES: &str = "region_names";
const LEGEND: &str = "legend";
const GROUP_COLORS: &str = "group_colors";
const GROUP_NAME: &str = "a group name or nothing";

/// Each function a mapping may define, with the most arguments it takes. A definition may take
/// fewer, and gets the leading ones.
const SIGNATURES: [(&str, usize); 11] = [
    (STATE, 2),
    (DETAIL, 2),
    (STEP, 3),
    (GUARD, 3),
    (TIMER, 3),
    (CAUSE, 3),
    (GROUP, 2),
    (READER_ACTIONS, 0),
    (REGION_NAMES, 0),
    (LEGEND, 0),
    (GROUP_COLORS, 0),
];
const LEGEND_KEYS: [&str; 3] = ["works", "wrong", "fixed"];
const REGION_COUNT: usize = 2;
const GUARD_BRACKETS: [char; 2] = ['[', ']'];

const ENABLED: &str = "enabled";
const SETTLED: &str = "settled";
const VARIABLES: &str = r"\bVARIABLES?\s+(\w+(?:\s*,\s*\w+)*)";
const DEFINITION: &str = r"^(\w+)\s*(?:\([^)]*\))?\s*==([\s\S]*)";
const PRIMES_OR_UNCHANGED: &str = r"[\w)\]>}]'|\bUNCHANGED\b";
const BLOCK_COMMENT: &str = r"\(\*[\s\S]*?\*\)";
const LINE_COMMENT: &str = r"\\\*.*";
const TOP_LEVEL: &str = r"\n(\S)";

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("cannot read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{name}: {message}")]
    Parse { name: String, message: String },
    #[error(
        "{name}: a mapping holds only functions, so {what} never runs. Move it into the function that uses it."
    )]
    Statement { name: String, what: String },
    #[error("{name}: the mapping must define `fn state(v, facts)`")]
    NoState { name: String },
    #[error("{name}: `fn {function}` takes at most {most} arguments, not {given}")]
    Arity {
        name: String,
        function: &'static str,
        most: usize,
        given: usize,
    },
    #[error("{name}: {function}() failed: {message}")]
    Call {
        name: String,
        function: &'static str,
        message: String,
    },
    #[error("{name}: {function}() must return {expected}; got {got}")]
    Returned {
        name: String,
        function: &'static str,
        expected: &'static str,
        got: String,
    },
}

/// The legend's words for each status, where the mapping gives them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Legend {
    pub works: Option<String>,
    pub wrong: Option<String>,
    pub fixed: Option<String>,
}

#[derive(Debug)]
pub struct Mapping {
    name: String,
    engine: Engine,
    ast: AST,
    arity: HashMap<&'static str, usize>,
    pub reader_actions: BTreeSet<String>,
    pub region_names: Option<[String; REGION_COUNT]>,
    pub legend: Legend,
    /// Each group the mapping colours, in its order, with its colour or none for the next free one.
    pub group_colors: Vec<(String, Option<String>)>,
}

impl Mapping {
    pub fn load(path: &Path) -> Result<Self, MappingError> {
        let source = fs::read_to_string(path).map_err(|source| MappingError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&path.display().to_string(), &source)
    }

    pub fn parse(name: &str, source: &str) -> Result<Self, MappingError> {
        let engine = restricted_engine(&LIMITS, HINT);
        let ast = engine
            .compile(source)
            .map_err(|error| MappingError::Parse {
                name: name.to_owned(),
                message: error.to_string(),
            })?;
        if let Some(statement) = ast.statements().first() {
            return Err(MappingError::Statement {
                name: name.to_owned(),
                what: format!("the statement at {}", statement.position()),
            });
        }
        let arity = arities(name, &ast)?;
        if !arity.contains_key(STATE) {
            return Err(MappingError::NoState {
                name: name.to_owned(),
            });
        }
        let mut mapping = Self {
            name: name.to_owned(),
            engine,
            ast,
            arity,
            reader_actions: BTreeSet::new(),
            region_names: None,
            legend: Legend::default(),
            group_colors: Vec::new(),
        };
        mapping.reader_actions = match mapping.constant(READER_ACTIONS)? {
            Some(value) => mapping
                .strings(READER_ACTIONS, value, "an array of action names")?
                .into_iter()
                .collect(),
            None => BTreeSet::new(),
        };
        mapping.region_names = mapping.region_names_of()?;
        mapping.legend = mapping.legend_of()?;
        mapping.group_colors = mapping.group_colors_of()?;
        Ok(mapping)
    }

    pub fn has_detail(&self) -> bool {
        self.arity.contains_key(DETAIL)
    }

    pub fn has_group(&self) -> bool {
        self.arity.contains_key(GROUP)
    }

    pub fn has_cause(&self) -> bool {
        self.arity.contains_key(CAUSE)
    }

    /// Whether only the reader's own actions are among `enabled`, so nothing happens until the
    /// reader acts.
    pub fn settled<'a>(&self, mut enabled: impl Iterator<Item = &'a str>) -> bool {
        enabled.all(|action| self.reader_actions.contains(action))
    }

    pub fn state(&self, vars: &Dynamic, facts: &Dynamic) -> Result<String, MappingError> {
        let value = self.call(STATE, [vars.clone(), facts.clone()])?;
        self.text(STATE, value)
    }

    /// The developer's label, or `None` when the mapping has no `detail`.
    pub fn detail(&self, vars: &Dynamic, facts: &Dynamic) -> Result<Option<String>, MappingError> {
        if !self.has_detail() {
            return Ok(None);
        }
        let value = self.call(DETAIL, [vars.clone(), facts.clone()])?;
        Ok(Some(display(&value)))
    }

    /// The step's words; the action itself when the mapping has no `step`. Empty draws no label.
    pub fn step(
        &self,
        action: &str,
        before: &Dynamic,
        after: &Dynamic,
    ) -> Result<String, MappingError> {
        if !self.arity.contains_key(STEP) {
            return Ok(action.to_owned());
        }
        let value = self.call(STEP, [action.into(), before.clone(), after.clone()])?;
        self.text(STEP, value)
    }

    /// When the step can happen, without brackets; empty when the mapping says nothing.
    pub fn guard(
        &self,
        action: &str,
        before: &Dynamic,
        after: &Dynamic,
    ) -> Result<String, MappingError> {
        if !self.arity.contains_key(GUARD) {
            return Ok(String::new());
        }
        let value = self.call(GUARD, [action.into(), before.clone(), after.clone()])?;
        let words = if value.is_unit() {
            String::new()
        } else {
            display(&value)
        };
        Ok(words.replace(GUARD_BRACKETS, "").trim().to_owned())
    }

    /// How many seconds the code waits before a timer step, or `None` for any other step.
    pub fn timer(
        &self,
        action: &str,
        before: &Dynamic,
        after: &Dynamic,
    ) -> Result<Option<f64>, MappingError> {
        if !self.arity.contains_key(TIMER) {
            return Ok(None);
        }
        let value = self.call(TIMER, [action.into(), before.clone(), after.clone()])?;
        if value.is_unit()
            || value
                .clone()
                .into_immutable_string()
                .is_ok_and(|s| s.is_empty())
        {
            return Ok(None);
        }
        let seconds = match (value.as_int(), value.as_float()) {
            (Ok(whole), _) => Some(whole as f64),
            (_, Ok(fraction)) => Some(fraction),
            _ => None,
        };
        match seconds {
            Some(seconds) if seconds.is_finite() && seconds > 0.0 => Ok(Some(seconds)),
            _ => Err(self.returned(TIMER, "a number of seconds above 0, or nothing", &value)),
        }
    }

    /// The hidden reason a bad state is bad, or `None` when the end state stands in for it.
    pub fn cause(
        &self,
        vars: &Dynamic,
        facts: &Dynamic,
        rule: &str,
    ) -> Result<Option<String>, MappingError> {
        if !self.has_cause() {
            return Ok(None);
        }
        let value = self.call(CAUSE, [vars.clone(), facts.clone(), rule.into()])?;
        Ok(present(&value))
    }

    /// The group a model state is in, or `None`.
    pub fn group(&self, vars: &Dynamic, facts: &Dynamic) -> Result<Option<String>, MappingError> {
        if !self.has_group() {
            return Ok(None);
        }
        let value = self.call(GROUP, [vars.clone(), facts.clone()])?;
        if !value.is_unit() && !value.is_string() {
            return Err(self.returned(GROUP, GROUP_NAME, &value));
        }
        Ok(present(&value))
    }

    fn call<const N: usize>(
        &self,
        function: &'static str,
        arguments: [Dynamic; N],
    ) -> Result<Dynamic, MappingError> {
        let given = self.arity.get(function).copied().unwrap_or_default();
        let arguments: Vec<Dynamic> = arguments.into_iter().take(given).collect();
        let options = CallFnOptions::new().eval_ast(false).rewind_scope(true);
        self.engine
            .call_fn_with_options::<Dynamic>(
                options,
                &mut Scope::new(),
                &self.ast,
                function,
                arguments,
            )
            .map_err(|error| MappingError::Call {
                name: self.name.clone(),
                function,
                message: error.to_string(),
            })
    }

    fn constant(&self, function: &'static str) -> Result<Option<Dynamic>, MappingError> {
        if !self.arity.contains_key(function) {
            return Ok(None);
        }
        self.call(function, []).map(Some)
    }

    fn text(&self, function: &'static str, value: Dynamic) -> Result<String, MappingError> {
        value
            .clone()
            .into_string()
            .map_err(|_| self.returned(function, "a string", &value))
    }

    fn strings(
        &self,
        function: &'static str,
        value: Dynamic,
        expected: &'static str,
    ) -> Result<Vec<String>, MappingError> {
        let fail = || self.returned(function, expected, &value);
        value
            .clone()
            .into_typed_array::<Dynamic>()
            .map_err(|_| fail())?
            .into_iter()
            .map(|item| item.into_string().map_err(|_| fail()))
            .collect()
    }

    fn region_names_of(&self) -> Result<Option<[String; REGION_COUNT]>, MappingError> {
        const EXPECTED: &str = "two names: the contexts', then the progress'";
        let Some(value) = self.constant(REGION_NAMES)? else {
            return Ok(None);
        };
        let names = self.strings(REGION_NAMES, value.clone(), EXPECTED)?;
        names
            .try_into()
            .map(Some)
            .map_err(|_| self.returned(REGION_NAMES, EXPECTED, &value))
    }

    fn legend_of(&self) -> Result<Legend, MappingError> {
        const EXPECTED: &str = "a map of works, wrong and fixed to words";
        let Some(value) = self.constant(LEGEND)? else {
            return Ok(Legend::default());
        };
        let fail = || self.returned(LEGEND, EXPECTED, &value);
        let mut words = value.clone().try_cast::<RhaiMap>().ok_or_else(fail)?;
        let mut take = |key: &str| match words.remove(key) {
            None => Ok(None),
            Some(word) if word.is_unit() => Ok(None),
            Some(word) => word.into_string().map(Some).map_err(|_| fail()),
        };
        let [works, wrong, fixed] = LEGEND_KEYS.map(&mut take);
        if !words.is_empty() {
            return Err(fail());
        }
        Ok(Legend {
            works: works?,
            wrong: wrong?,
            fixed: fixed?,
        })
    }

    fn group_colors_of(&self) -> Result<Vec<(String, Option<String>)>, MappingError> {
        const EXPECTED: &str = r##"an array of [group, "#rrggbb" or ()] pairs"##;
        let Some(value) = self.constant(GROUP_COLORS)? else {
            return Ok(Vec::new());
        };
        let fail = || self.returned(GROUP_COLORS, EXPECTED, &value);
        let pairs = value.clone().try_cast::<Array>().ok_or_else(fail)?;
        pairs
            .into_iter()
            .map(|pair| {
                let [group, color]: [Dynamic; 2] = pair
                    .try_cast::<Array>()
                    .and_then(|pair| pair.try_into().ok())
                    .ok_or_else(fail)?;
                let group = group.into_string().map_err(|_| fail())?;
                let color = if color.is_unit() {
                    None
                } else {
                    Some(color.into_string().map_err(|_| fail())?)
                };
                Ok((group, color))
            })
            .collect()
    }

    fn returned(
        &self,
        function: &'static str,
        expected: &'static str,
        got: &Dynamic,
    ) -> MappingError {
        MappingError::Returned {
            name: self.name.clone(),
            function,
            expected,
            got: shown(got),
        }
    }
}

/// One state's variables as the mapping reads them.
pub fn value(vars: &Vars) -> Dynamic {
    to_dynamic(vars).unwrap_or_default()
}

/// What the mapping knows of a state beyond its variables: the actions it enables, and whether
/// only the reader's actions are among them.
pub fn facts<'a>(enabled: impl IntoIterator<Item = &'a str>, settled: bool) -> Dynamic {
    let enabled: Array = enabled
        .into_iter()
        .map(|action| ImmutableString::from(action).into())
        .collect();
    let mut facts = RhaiMap::new();
    facts.insert(ENABLED.into(), enabled.into());
    facts.insert(SETTLED.into(), settled.into());
    facts.into()
}

/// A mapping to start from, for a spec that has none: every label reads as the raw values, and
/// every step as its action, until someone writes the reader's words.
pub fn scaffold(name: &str, spec: &str) -> String {
    let text = without_comments(spec);
    let variables: Vec<&str> = Regex::new(VARIABLES)
        .expect("valid regex")
        .captures_iter(&text)
        .flat_map(|found| found.get(1).map_or("", |list| list.as_str()).split(','))
        .map(str::trim)
        .collect();
    let actions = actions(&text);
    let steps: String = actions
        .iter()
        .map(|action| format!("\t\t{}: {},\n", Json::from(*action), Json::from(*action)))
        .collect();
    format!(
        r##"// Chart labels for {name}, for pstack.statechart. Write labels in plain words, not variable or action names.
// Rhai, run in a sandbox: only functions, and each defined function is one the chart calls.
// v, before, and after map each variable to its value. Strings, numbers, and booleans keep their kind.
// Sequences, tuples, and sets become arrays, indexed from 0. Records and functions become maps.
// Variables: {variables}.

// The actions the reader takes. facts.settled is true when only these are enabled, and the overview
// keeps the shortest path to each settled state, plus every bug path.
fn reader_actions() {{
	[]
}}

// The label for one model state. facts.enabled is the array of actions the state enables,
// including steps that change no variable. Write it as "context: progress", such as
// "Offline: upload queued": the chart puts the states of one context in one box.
// Do not claim a result that a pending step can still change, such as "upload failed" while a
// retry is still queued.
fn state(v, facts) {{
	v.to_json()
}}

// The names of the two parallel regions, when the overview draws them: contexts, then progress.
// fn region_names() {{ ["Network", "What happened to the file"] }}

// The group of one model state, in words the reader sees, such as "Upload done". The chart fills
// the states of one group with one color, and the legend names the groups. The model checker still
// decides which states break a rule, and draws them with a thick red border on top of the group color.
// group_colors gives a group a light color, in legend order. Groups without one take a palette color.
// fn group(v, facts) {{}}
// fn group_colors() {{ [["Upload done", "#d1f2eb"]] }}

// The label for one step. A step that leaves the reader in the same state is not drawn,
// unless the reader takes it on a bug path. "" draws no label, and a numbered bug step shows the action.
fn step(action, before, after) {{
	let steps = #{{
{steps}	}};
	steps[action] ?? action
}}

// When the step can happen, in plain words, such as "offline or disk full". The chart shows
// "step [words]". "" adds nothing.
// fn guard(action, before, after) {{}}

// For a timer step, how many seconds the code waits, from the constant in the code. The chart
// shows "after N s, step". Return nothing for other steps.
// fn timer(action, before, after) {{}}

// Why a bad state is bad, in words a product manager reads, such as "the laptop went offline, and
// the app drops an upload it cannot send". No variable, function, or event names.
// Counterexamples that break one rule with one cause are one bug, so two bugs that end on the
// same screen stay apart only if their causes differ. Return () to group by the end state.
// fn cause(v, facts, invariant) {{}}

// The developer's words for one model state. With it, each bug is also drawn through these states.
// fn detail(v, facts) {{}}

// The legend's words for each status, in place of the defaults. Group entries keep their names.
// fn legend() {{ #{{ works: "Works on main", wrong: "Wrong on main", fixed: "Fixed in the open PR" }} }}
"##,
        variables = variables.join(", "),
    )
}

fn without_comments(spec: &str) -> String {
    let blocks = Regex::new(BLOCK_COMMENT).expect("valid regex");
    let lines = Regex::new(LINE_COMMENT).expect("valid regex");
    lines
        .replace_all(&blocks.replace_all(spec, " "), " ")
        .into_owned()
}

/// Every definition that primes a variable or leaves some unchanged, sorted: the actions. A
/// definition only another action calls is listed too, though no step is named after it.
fn actions(text: &str) -> Vec<&str> {
    let definition = Regex::new(DEFINITION).expect("valid regex");
    let changes = Regex::new(PRIMES_OR_UNCHANGED).expect("valid regex");
    let starts = Regex::new(TOP_LEVEL).expect("valid regex");
    let mut bounds: Vec<usize> = starts
        .captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| m.start())
        .collect();
    bounds.insert(0, 0);
    bounds.push(text.len());
    let found: BTreeSet<&str> = bounds
        .windows(2)
        .filter_map(|pair| definition.captures(&text[pair[0]..pair[1]]))
        .filter(|captures| changes.is_match(&captures[2]))
        .filter_map(|captures| captures.get(1).map(|name| name.as_str()))
        .collect();
    found.into_iter().collect()
}

fn arities(name: &str, ast: &AST) -> Result<HashMap<&'static str, usize>, MappingError> {
    let mut arity = HashMap::new();
    for function in ast.iter_functions() {
        let Some(&(known, most)) = SIGNATURES.iter().find(|(known, _)| *known == function.name)
        else {
            continue;
        };
        let given = function.params.len();
        if given > most {
            return Err(MappingError::Arity {
                name: name.to_owned(),
                function: known,
                most,
                given,
            });
        }
        let entry = arity.entry(known).or_insert(given);
        *entry = (*entry).max(given);
    }
    Ok(arity)
}

fn present(value: &Dynamic) -> Option<String> {
    if value.is_unit() {
        return None;
    }
    let text = display(value);
    (!text.is_empty()).then_some(text)
}

fn display(value: &Dynamic) -> String {
    value
        .clone()
        .into_string()
        .unwrap_or_else(|_| value.to_string())
}

fn shown(value: &Dynamic) -> String {
    if value.is_unit() {
        return "nothing".to_owned();
    }
    let text = value.to_string();
    if value.is_string() {
        format!("{text:?}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::scripts::statechart::model::explore;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statechart");
    const NAME: &str = "Test.map.rhai";
    const MUST_PARSE: &str = "the mapping must parse";
    const MUST_CALL: &str = "the mapping must run";
    const MUST_EXPLORE: &str = "the model must explore";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(FIXTURES).join(name)
    }

    fn mapping(source: &str) -> Mapping {
        Mapping::parse(NAME, source).expect(MUST_PARSE)
    }

    fn vars(json: Json) -> Dynamic {
        let Json::Object(vars) = json else {
            panic!("vars are an object");
        };
        value(&vars)
    }

    #[test]
    fn output_folder_labels_its_first_state_as_upstream() {
        let model = explore(&fixture("OutputFolder.tla"), &fixture("OutputFolder.cfg"))
            .expect(MUST_EXPLORE);
        let mapping = Mapping::load(&fixture("OutputFolder.map.rhai")).expect(MUST_PARSE);
        let first = value(&model.states[model.initial[0]]);
        let facts = facts(["Start"], true);
        assert_eq!(
            mapping.state(&first, &facts).expect(MUST_CALL),
            "Before any run: no folder"
        );
        assert_eq!(
            mapping.group(&first, &facts).expect(MUST_CALL).as_deref(),
            Some("Not started")
        );
        assert_eq!(
            mapping
                .cause(&first, &facts, "KeepsYourFiles")
                .expect(MUST_CALL)
                .as_deref(),
            Some("a run deleted a folder that had your notes in it")
        );
        assert_eq!(
            mapping.cause(&first, &facts, "Unlisted").expect(MUST_CALL),
            None
        );
    }

    #[test]
    fn output_folder_says_every_step_upstream_says() {
        let model = explore(&fixture("OutputFolder.tla"), &fixture("OutputFolder.cfg"))
            .expect(MUST_EXPLORE);
        let mapping = Mapping::load(&fixture("OutputFolder.map.rhai")).expect(MUST_PARSE);
        let values: Vec<Dynamic> = model.states.iter().map(value).collect();
        let said: BTreeSet<String> = model
            .steps
            .iter()
            .map(|step| {
                let action = step.action.as_deref().expect("every step is named");
                mapping
                    .step(action, &values[step.from], &values[step.to])
                    .expect(MUST_CALL)
            })
            .collect();
        for words in [
            "you start run 1",
            "you start run 2 in a second shell",
            "run 1 says the input is wrong",
            "run 2 moves its folder into place",
            "you kill run 1's Node, and its TLC keeps running",
            "",
        ] {
            assert!(said.contains(words), "{words:?} not among {said:?}");
        }
        assert!(said.iter().all(|words| !words.contains("()")), "{said:?}");
    }

    #[test]
    fn constants_read_as_upstream_exports() {
        let mapping = mapping(
            r##"
            fn state(v) { "s" }
            fn reader_actions() { ["Start", "Edit"] }
            fn region_names() { ["Network", "Progress"] }
            fn legend() { #{ works: "Works on main" } }
            fn group_colors() { [["Done", "#d1f2eb"], ["Busy", ()]] }
            "##,
        );
        assert_eq!(
            mapping.reader_actions,
            BTreeSet::from(["Edit".to_owned(), "Start".to_owned()])
        );
        assert_eq!(
            mapping.region_names,
            Some(["Network".to_owned(), "Progress".to_owned()])
        );
        assert_eq!(
            mapping.legend,
            Legend {
                works: Some("Works on main".to_owned()),
                ..Legend::default()
            }
        );
        assert_eq!(
            mapping.group_colors,
            vec![
                ("Done".to_owned(), Some("#d1f2eb".to_owned())),
                ("Busy".to_owned(), None)
            ]
        );
        assert!(mapping.settled(["Start", "Edit"].into_iter()));
        assert!(!mapping.settled(["Start", "Tick"].into_iter()));
    }

    #[test]
    fn missing_functions_fall_back_as_upstream() {
        let mapping = mapping("fn state(v, facts) { `${facts.settled}` }");
        let none = vars(json!({}));
        assert_eq!(mapping.step("Pay", &none, &none).expect(MUST_CALL), "Pay");
        assert_eq!(mapping.guard("Pay", &none, &none).expect(MUST_CALL), "");
        assert_eq!(mapping.timer("Pay", &none, &none).expect(MUST_CALL), None);
        assert_eq!(mapping.cause(&none, &none, "Rule").expect(MUST_CALL), None);
        assert_eq!(mapping.detail(&none, &none).expect(MUST_CALL), None);
        assert_eq!(
            mapping.state(&none, &facts([], true)).expect(MUST_CALL),
            "true"
        );
    }

    #[test_case("fn guard(a, b, c) { \" [offline] \" }" => "offline"; "brackets and space go")]
    #[test_case("fn guard(a) { () }" => ""; "nothing is no words")]
    #[test_case("fn guard(a) { 3 }" => "3"; "a number reads as text")]
    fn guard_words(source: &str) -> String {
        let mapping = mapping(&format!("fn state(v) {{ \"s\" }}\n{source}"));
        let none = vars(json!({}));
        mapping.guard("Retry", &none, &none).expect(MUST_CALL)
    }

    #[test_case("5" => Some(5.0); "whole seconds")]
    #[test_case("1.5" => Some(1.5); "fractions")]
    #[test_case("()" => None; "nothing")]
    #[test_case("\"\"" => None; "empty text")]
    fn timer_seconds(returned: &str) -> Option<f64> {
        let mapping = mapping(&format!(
            "fn state(v) {{ \"s\" }}\nfn timer(action) {{ {returned} }}"
        ));
        let none = vars(json!({}));
        mapping.timer("Retry", &none, &none).expect(MUST_CALL)
    }

    #[test_case("0"; "zero")]
    #[test_case("-1.5"; "negative")]
    #[test_case("\"soon\""; "words")]
    fn timer_refuses_what_is_not_a_wait(returned: &str) {
        let mapping = mapping(&format!(
            "fn state(v) {{ \"s\" }}\nfn timer(action) {{ {returned} }}"
        ));
        let none = vars(json!({}));
        let error = mapping.timer("Retry", &none, &none).expect_err("refused");
        assert!(
            matches!(
                error,
                MappingError::Returned {
                    function: TIMER,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test_case("" => matches MappingError::NoState { .. }; "no state")]
    #[test_case("fn state(v) { \"s\" }\nlet quiet = 1;" => matches MappingError::Statement { .. }; "top level statement")]
    #[test_case("fn state(v, facts, extra) { \"s\" }" => matches MappingError::Arity { function: STATE, most: 2, given: 3, .. }; "too many arguments")]
    #[test_case("fn state(v) { \"s\" " => matches MappingError::Parse { .. }; "parse error")]
    #[test_case("fn state(v) { \"s\" }\nfn legend() { #{ broken: \"x\" } }" => matches MappingError::Returned { function: LEGEND, .. }; "unknown legend key")]
    #[test_case("fn state(v) { \"s\" }\nfn region_names() { [\"one\"] }" => matches MappingError::Returned { function: REGION_NAMES, .. }; "one region name")]
    #[test_case("fn state(v) { \"s\" }\nfn reader_actions() { [1] }" => matches MappingError::Returned { function: READER_ACTIONS, .. }; "action that is not a name")]
    #[test_case("fn state(v) { \"s\" }\nfn group_colors() { #{ Done: \"#fff\" } }" => matches MappingError::Returned { function: GROUP_COLORS, .. }; "colors as a map lose their order")]
    fn refused_mapping(source: &str) -> MappingError {
        Mapping::parse(NAME, source).expect_err("refused")
    }

    #[test_case("v.count" => "1"; "a number")]
    #[test_case("v.missing" => "nothing"; "a variable the state lacks")]
    fn state_must_be_text(body: &str) -> String {
        let mapping = mapping(&format!("fn state(v) {{ {body} }}"));
        let error = mapping
            .state(&vars(json!({"count": 1})), &facts([], false))
            .expect_err("refused");
        let MappingError::Returned { function, got, .. } = error else {
            panic!("refused for what it returned: {error}");
        };
        assert_eq!(function, STATE);
        got
    }

    #[test]
    fn a_runaway_loop_stops() {
        let mapping = mapping("fn state(v) { loop {} }");
        let error = mapping
            .state(&vars(json!({})), &facts([], false))
            .expect_err("refused");
        assert!(
            matches!(
                error,
                MappingError::Call {
                    function: STATE,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn scaffold_lists_variables_and_actions_and_runs() {
        let spec = fs::read_to_string(fixture("OutputFolder.tla")).expect("fixture");
        let source = scaffold("OutputFolder", &spec);
        assert!(source.contains(
            "// Variables: pc, inputOk, snap, read, out, aside, spec, noted, wrote, broke, exitCode."
        ));
        assert!(source.contains("\t\t\"Start\": \"Start\",\n"), "{source}");
        let mapping = mapping(&source);
        let model = explore(&fixture("OutputFolder.tla"), &fixture("OutputFolder.cfg"))
            .expect(MUST_EXPLORE);
        let first = value(&model.states[model.initial[0]]);
        let label = mapping.state(&first, &facts([], false)).expect(MUST_CALL);
        assert!(
            label.starts_with('{') && label.contains("\"pc\""),
            "{label}"
        );
        assert_eq!(
            mapping.step("Start", &first, &first).expect(MUST_CALL),
            "Start"
        );
        assert_eq!(
            mapping.step("Unknown", &first, &first).expect(MUST_CALL),
            "Unknown"
        );
    }
}
