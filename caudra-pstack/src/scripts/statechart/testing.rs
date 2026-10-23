//! What the chart tests share: a spec explored from text, as upstream's tests write one to a
//! temporary folder and run TLC on it.

use std::fs;

use super::chart::{ChartError, ChartOptions, Charter, Charts, DrawOptions, Drawing, draw};
use super::mapping::Mapping;
use super::mermaid::parse;
use super::model::{Model, explore};
use super::text::rule_sentences;

const MAPPING_NAME: &str = "Test.map.rhai";

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
