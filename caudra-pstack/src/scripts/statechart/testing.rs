//! What the chart tests share: a spec explored from text, as upstream's tests write one to a
//! temporary folder and run TLC on it.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use regex::Regex;

use super::chart::{
    BREAK, ChartError, ChartOptions, Charter, Charts, DrawOptions, Drawing, draw, draw_with,
};
use super::mapping::Mapping;
use super::mermaid::parse;
use super::model::{Model, explore};
use super::second::{Role, SecondModel};
use super::text::rule_sentences;

const MAPPING_NAME: &str = "Test.map.rhai";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statechart");

pub(super) const PAY_MAP: &str = r#"
    fn reader_actions() { ["Pay"] }
    fn state(v) { v.screen + ["", ", charged", ", charged twice"][v.charges] }
    fn step(a) { #{ Pay: "you pay", Charge: "the bank charges you", Refund: "the bank refunds you" }[a] ?? "" }
    fn cause() { "each charge request charges again" }
"#;
pub(super) const PAY_CFG: &str =
    "SPECIFICATION Spec\n\\* The reader's rule.\nINVARIANTS\n  NoDoubleCharge\n";
pub(super) const CHARGE_AGAIN: &str =
    r#"screen = "Paying" /\ charges < 2 /\ charges' = charges + 1 /\ UNCHANGED screen"#;

pub(super) struct Spec {
    pub(super) model: Model,
    pub(super) mapping: Mapping,
    pub(super) options: DrawOptions,
}

impl Spec {
    /// Explores module `name` from `tla` under `cfg`, labelled by the Rhai `mapping`.
    pub(super) fn new(name: &str, tla: &str, cfg: &str, mapping: &str) -> Self {
        Self {
            model: explore_text(name, tla, cfg),
            mapping: Mapping::parse(MAPPING_NAME, mapping).expect("the mapping parses"),
            options: DrawOptions {
                rules: rule_sentences(tla),
                ..DrawOptions::default()
            },
        }
    }

    /// What one `from-spec` run writes.
    pub(super) fn draw(&self) -> Charts {
        draw(&self.model, &self.mapping, &self.options).expect("the charts draw")
    }

    /// What one `tlc-to-chart.mjs` run draws, or why it refuses.
    pub(super) fn try_chart(&self, options: &ChartOptions) -> Result<Drawing, ChartError> {
        Charter::new(&self.model, &self.mapping, &self.options.rules)?.chart(options)
    }

    pub(super) fn chart(&self, options: &ChartOptions) -> Drawing {
        self.try_chart(options).expect("the chart draws")
    }

    /// What one `from-spec` run with `--fixed` or `--against` writes.
    pub(super) fn draw_with(&self, second: &SecondModel) -> Charts {
        draw_with(&self.model, Some(second), &self.mapping, &self.options).expect("the charts draw")
    }

    /// What one `tlc-to-chart.mjs` run with `--fixed-dot` or `--against-dot` draws, or why it
    /// refuses.
    pub(super) fn try_chart_with(
        &self,
        second: &SecondModel,
        options: &ChartOptions,
    ) -> Result<Drawing, ChartError> {
        Charter::with_second(
            &self.model,
            Some(second),
            &self.mapping,
            &self.options.rules,
        )?
        .chart(options)
    }

    pub(super) fn chart_with(&self, second: &SecondModel, options: &ChartOptions) -> Drawing {
        self.try_chart_with(second, options)
            .expect("the chart draws")
    }
}

/// The open fix of a spec, by upstream's default name.
pub(super) fn fix(model: &Model) -> SecondModel<'_> {
    SecondModel {
        model,
        role: Role::Fix { label: None },
    }
}

/// A file of the chart tests' fixtures.
pub(super) fn fixture(name: &str) -> PathBuf {
    PathBuf::from(FIXTURES).join(name)
}

/// Explores module `name` from `tla` under `cfg`.
pub(super) fn explore_text(name: &str, tla: &str, cfg: &str) -> Model {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let spec = dir.path().join(format!("{name}.tla"));
    let config = dir.path().join(format!("{name}.cfg"));
    fs::write(&spec, tla).expect("the spec writes");
    fs::write(&config, cfg).expect("the cfg writes");
    explore(&spec, &config).expect("the model explores")
}

/// A config that checks `rule` alone, as upstream's `--expect-broken` runs do.
pub(super) fn rule_cfg(rule: &str) -> String {
    format!("SPECIFICATION Spec\nINVARIANT {rule}\n")
}

/// Each arrow of a chart as the machine it draws, sorted: "from -> to : label".
pub(super) fn arrows(mermaid: &str) -> Vec<String> {
    let chart = parse(mermaid).expect("the chart reads back");
    let mut edges: Vec<String> = chart
        .regions
        .iter()
        .flat_map(|(_, machine)| &machine.transitions)
        .map(|t| format!("{} -> {} : {}", t.from, t.to, t.label))
        .collect();
    edges.sort();
    edges
}

/// The chart's lines, with each `<br/>` read as a space.
pub(super) fn unwrapped_lines(mermaid: &str) -> Vec<String> {
    mermaid
        .replace("<br/>", " ")
        .lines()
        .map(str::to_owned)
        .collect()
}

pub(super) fn pay(module: &str, charge: &str, wait: bool) -> String {
    let next = if wait {
        "Wait == screen = \"Paying\" /\\ UNCHANGED vars\nNext =="
    } else {
        "Next =="
    };
    format!(
        r#"---- MODULE {module} ----
EXTENDS Naturals
VARIABLES screen, charges
vars == <<screen, charges>>
Init == screen = "Cart" /\ charges = 0
Pay == screen = "Cart" /\ screen' = "Paying" /\ UNCHANGED charges
Charge == {charge}
Refund == charges = 2 /\ charges' = 1 /\ screen' = "Refunded"
Next == Pay \/ Charge \/ Refund
Spec == Init /\ [][Next]_vars
NoDoubleCharge == charges <= 1
===="#
    )
    .replace("Next ==", next)
}

/// Each drawn state's name, with `<br/>` read as a space, and its class.
pub(super) fn status_of(mermaid: &str) -> BTreeMap<String, String> {
    let state = Regex::new(r#"(?m)^ {2}state "([^"]+)" as (s\d+)$"#).expect("a pattern");
    let class = Regex::new(r"(?m)^ {2}class (s\d+) (\w+)$").expect("a pattern");
    let names: HashMap<&str, String> = state
        .captures_iter(mermaid)
        .map(|c| {
            (
                c.get(2).map_or("", |m| m.as_str()),
                c[1].replace(BREAK, " "),
            )
        })
        .collect();
    class
        .captures_iter(mermaid)
        .map(|c| (names[&c[1]].clone(), c[2].to_owned()))
        .collect()
}
