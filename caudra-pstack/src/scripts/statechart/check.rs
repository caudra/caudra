//! Upstream's `check`: what one model's config checks, and the report of checking it. Upstream
//! runs TLC once per rule and once more for the state graph; tla-checker explores once, in
//! process, and runs once more per `PROPERTY`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::Value as Json;

use super::cli::{CannotFinish, Report};
use super::model::{Model, Vars, explore, property_counterexample};
use super::publish::CHECKS;
use crate::host::Host;

const CFG_EXTENSION: &str = ".cfg";
const OUT_EXTENSION: &str = ".out";
const INVARIANT: &str = "INVARIANT";
const PROPERTY: &str = "PROPERTY";
const CHECKER: &str = "tla-checker";

static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(SPECIFICATION|INIT|NEXT|CONSTANTS?|INVARIANTS?|PROPERTY|PROPERTIES|SYMMETRY|VIEW|CONSTRAINTS?|ACTION_CONSTRAINTS?|CHECK_DEADLOCK|POSTCONDITION|ALIAS)$")
        .expect("valid regex")
});
static INVARIANT_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^INVARIANTS?$").expect("valid regex"));
static PROPERTY_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^PROPERT(Y|IES)$").expect("valid regex"));
static BLOCK_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\*[\s\S]*?\*\)").expect("valid regex"));
static LINE_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\\*.*").expect("valid regex"));
static NEXT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^NEXT\s+(\w+)").expect("valid regex"));
static SPECIFICATION_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^SPECIFICATION\s+(\w+)").expect("valid regex"));
static BOXED_NEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\]\s*\[\s*(\w+)\s*\]_").expect("valid regex"));

/// A config's rules taken out of it: what is left checks no rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgRules {
    pub without_rules: String,
    /// The `INVARIANT` rules, in the config's order.
    pub rules: Vec<String>,
    /// The `PROPERTY` rules that are not also an `INVARIANT`.
    pub properties: Vec<String>,
}

impl CfgRules {
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.rules.iter().chain(&self.properties)
    }
}

/// One model and the config that checks it, as the reader named them and as they resolve.
#[derive(Debug, Clone)]
pub struct ModelInput {
    pub spec: String,
    pub cfg: String,
    pub spec_path: PathBuf,
    pub cfg_path: PathBuf,
    pub rules: CfgRules,
}

impl ModelInput {
    pub fn file_name(&self) -> String {
        file_name(&self.spec)
    }

    pub fn module(&self) -> String {
        self.file_name()
            .strip_suffix(".tla")
            .map_or_else(|| self.file_name(), str::to_owned)
    }
}

/// A model after its checks: its graph, and whether each rule holds.
#[derive(Debug)]
pub struct Checked {
    pub input: ModelInput,
    pub model: Model,
    /// Each rule and property with whether it broke, rules first.
    pub results: Vec<(String, bool)>,
}

impl Checked {
    pub fn broken(&self) -> impl Iterator<Item = &str> {
        self.results
            .iter()
            .filter(|(_, broken)| *broken)
            .map(|(rule, _)| rule.as_str())
    }

    pub fn held(&self) -> Vec<String> {
        self.results
            .iter()
            .filter(|(_, broken)| !broken)
            .map(|(rule, _)| rule.clone())
            .collect()
    }

    pub fn any_broken(&self) -> bool {
        self.broken().next().is_some()
    }

    pub fn any_rule_broken(&self) -> bool {
        self.input
            .rules
            .rules
            .iter()
            .any(|rule| self.model.broken(rule))
    }
}

pub fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Upstream's `splitRules`: the config without its comments and rule sections, and the rules.
/// TLC takes only a state predicate as an `INVARIANT`, and checks a state predicate given as a
/// `PROPERTY` only in the initial states, which the `INVARIANT` check covers too.
pub fn split_rules(cfg: &str) -> CfgRules {
    let uncommented = LINE_COMMENT
        .replace_all(&BLOCK_COMMENT.replace_all(cfg, " "), " ")
        .into_owned();
    let mut kept: Vec<String> = Vec::new();
    let mut rules: Vec<String> = Vec::new();
    let mut properties: Vec<String> = Vec::new();
    let mut section = "";
    for word in uncommented.split_whitespace() {
        let keyword = KEYWORD.is_match(word);
        if keyword {
            section = word;
        }
        let list = if INVARIANT_SECTION.is_match(section) {
            Some(&mut rules)
        } else if PROPERTY_SECTION.is_match(section) {
            Some(&mut properties)
        } else {
            None
        };
        match list {
            None if keyword && !kept.is_empty() => kept.push(format!("\n{word}")),
            None => kept.push(word.to_owned()),
            Some(list) => {
                if word != section && !list.iter().any(|known| known == word) {
                    list.push(word.to_owned());
                }
            }
        }
    }
    properties.retain(|property| !rules.contains(property));
    CfgRules {
        without_rules: format!("{}\n", kept.join(" ")),
        rules,
        properties,
    }
}

/// Explores `input`, checks each of its properties, and reports both: upstream's `check`.
/// `checks` is the run's folder for the configs each property check reads, shown as `shown`.
pub fn check(
    host: &dyn Host,
    input: ModelInput,
    checks: &Path,
    shown: &str,
    expected: &[String],
    report: &mut Report,
) -> Result<Checked, CannotFinish> {
    let started = host.now();
    let model = explore(&input.spec_path, &input.cfg_path)
        .map_err(|error| CannotFinish::new(format!("{}: {error}", input.spec)))?;
    warn_unnamed_steps(&input, &model, report);
    let mut runs = 1;
    report.say(&format!(
        "{CHECKER} on {} with {}:",
        input.file_name(),
        input.cfg
    ));
    let rules = &input.rules;
    if rules.rules.is_empty() {
        let consequence = if rules.properties.is_empty() {
            "no rule is checked"
        } else {
            "the chart marks no state broken"
        };
        report.say(&format!(
            "  {} has no {INVARIANT}, so {consequence}.",
            file_name(&input.cfg)
        ));
    }
    let said = |rule: &str, broken: bool| match (broken, expected.iter().any(|e| e == rule)) {
        (false, _) => format!("  {rule} holds."),
        (true, true) => format!("  {rule} is broken (expected)."),
        (true, false) => format!("  {rule} is broken."),
    };
    let mut results = Vec::new();
    for rule in &rules.rules {
        let broken = model.broken(rule);
        report.say(&said(rule, broken));
        results.push((rule.clone(), broken));
    }
    let module = input.module();
    for property in &rules.properties {
        runs += 1;
        let base = format!("{module}-{property}");
        let cfg = checks.join(format!("{base}{CFG_EXTENSION}"));
        let written = format!("{}{PROPERTY} {property}\n", rules.without_rules);
        fs::write(&cfg, written).map_err(|error| {
            CannotFinish::new(format!("cannot write {}: {error}", cfg.display()))
        })?;
        let counterexample = property_counterexample(&input.spec_path, &cfg)
            .map_err(|error| CannotFinish::new(format!("{}: {error}", input.spec)))?;
        let mut line = said(property, counterexample.is_some());
        if let Some(states) = &counterexample {
            let out = format!("{base}{OUT_EXTENSION}");
            fs::write(checks.join(&out), behavior(states))
                .map_err(|error| CannotFinish::new(format!("cannot write {out}: {error}")))?;
            let _ = write!(
                line,
                " The chart draws only {INVARIANT} rules, so read the counterexample in {}.",
                Path::new(shown).join(CHECKS).join(out).display()
            );
        }
        report.say(&line);
        results.push((property.clone(), counterexample.is_some()));
    }
    let seconds = host.now().duration_since(started).as_secs_f64();
    let times = if runs == 1 { "time" } else { "times" };
    report.say(&format!(
        "  {CHECKER} ran {runs} {times} in {seconds:.1} s."
    ));
    Ok(Checked {
        input,
        model,
        results,
    })
}

/// A counterexample as upstream's TLC output words it: each state, numbered, with its action.
fn behavior(states: &[(Option<Arc<str>>, Vars)]) -> String {
    states
        .iter()
        .enumerate()
        .map(|(index, (action, vars))| {
            let action = action.as_deref().unwrap_or(if index == 0 {
                "Initial predicate"
            } else {
                "Next"
            });
            format!(
                "State {}: <{action}>\n{}\n",
                index + 1,
                Json::Object(vars.clone())
            )
        })
        .collect()
}

/// The next-state action's name: the config's `NEXT`, or the `[][Next]_vars` of its
/// `SPECIFICATION`.
fn next_action(spec: &str, cfg: &str) -> Option<String> {
    if let Some(next) = NEXT_LINE.captures(cfg) {
        return Some(next[1].to_owned());
    }
    let text = LINE_COMMENT
        .replace_all(&BLOCK_COMMENT.replace_all(spec, " "), " ")
        .into_owned();
    let body = SPECIFICATION_LINE.captures(cfg).and_then(|found| {
        let definition = Regex::new(&format!(
            r"(?m)^{}\s*==(.*(\n[ \t]+.*)*)",
            regex::escape(&found[1])
        ))
        .ok()?;
        definition
            .captures(&text)
            .and_then(|body| body.get(1))
            .map(|body| body.as_str().to_owned())
    });
    BOXED_NEXT
        .captures(body.as_deref().unwrap_or(&text))
        .map(|next| next[1].to_owned())
}

fn warn_unnamed_steps(input: &ModelInput, model: &Model, report: &mut Report) {
    let spec = fs::read_to_string(&input.spec_path).unwrap_or_default();
    let Some(next) = next_action(&spec, &input.rules.without_rules) else {
        return;
    };
    let unnamed = model
        .steps
        .iter()
        .filter(|step| step.action.as_deref().is_none_or(|action| action == next))
        .count();
    if unnamed == 0 {
        return;
    }
    let steps = if unnamed == 1 {
        "step has"
    } else {
        "steps have"
    };
    report.warn(&format!(
        "{}: {unnamed} {steps} the name {next}, the whole next-state action, because {CHECKER} names a step after the innermost named action that takes it. If the reader must tell these steps apart, give each part of {next} its own name, as in Pay == ... and {next} == Pay \\/ Charge.",
        input.spec
    ));
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test]
    fn rules_come_out_of_the_config() {
        let rules = split_rules(
            "SPECIFICATION Spec \\* the spec\n(* old *)\nINVARIANTS\n  TypeOK\n  NoLoss\nPROPERTY NoLoss\nPROPERTIES Eventually\nCONSTANT N = 3\n",
        );

        assert_eq!(
            rules,
            CfgRules {
                without_rules: "SPECIFICATION Spec \nCONSTANT N = 3\n".to_owned(),
                rules: vec!["TypeOK".to_owned(), "NoLoss".to_owned()],
                properties: vec!["Eventually".to_owned()],
            }
        );
    }

    #[test_case("SPECIFICATION Spec\n", "Spec == Init /\\ [][Next]_vars\n", Some("Next"); "from the specification")]
    #[test_case("INIT Init\nNEXT Step\n", "", Some("Step"); "from the config")]
    #[test_case("SPECIFICATION Spec\n", "Spec == Init\n", None; "none")]
    fn the_next_state_action_is_named(cfg: &str, spec: &str, expected: Option<&str>) {
        assert_eq!(next_action(spec, cfg).as_deref(), expected);
    }
}
