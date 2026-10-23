//! What upstream's `tlc-to-chart.mjs` writes beside a chart: one reader story per bug, and the
//! counts of `summary.json`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value as Json, json};

use super::chart::{Chart, Flat, Status, View};
use super::ordered::OrderedSet;
use super::regions::RegionsChart;
use super::text::capitalize;

const ONLY_WAY: &str = "This is the only way to get here in the model.";
/// The key upstream names a bug's replay on the second model under when there is none.
const SECOND_KEY: &str = "fix";
const FLAT_STATUS: [Status; 3] = [Status::Works, Status::Wrong, Status::Fixed];

/// Labels and causes that read like code: camelCase, snake_case, or a call.
static CODE_LIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?-u:\b)[a-z]{2,}[A-Z][a-z]|(?-u:\b)[0-9A-Za-z_]+_[0-9A-Za-z_]+(?-u:\b)|(?-u:\b)[A-Za-z_][0-9A-Za-z_]*\(|(?-u:\b)[A-Z][a-z]+[A-Z][a-z]",
    )
    .expect("valid regex")
});

/// One story per bug: the rule, why, and each step the reader sees.
pub(super) fn stories(chart: &Chart) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (index, class) in chart.classes.iter().enumerate() {
        let trace = &chart.charter.model.traces[class.bug.trace];
        lines.push(format!("## {}. {}", index + 1, chart.show(class.end())));
        lines.push(String::new());
        lines.push(format!("Rule broken: {}", rule_of(chart, &trace.rule)));
        if let Some(cause) = class.bug.cause.as_deref().filter(|cause| !cause.is_empty()) {
            lines.push(format!(
                "Why: {}.",
                cause.strip_suffix('.').unwrap_or(cause)
            ));
        }
        lines.push(if class.bug.count == 1 {
            ONLY_WAY.to_owned()
        } else {
            format!(
                "There are {} ways to get here in the model. This is the shortest.",
                class.bug.count
            )
        });
        lines.push(String::new());
        for (step, at) in class.path.iter().enumerate() {
            lines.push(if step == 0 {
                format!("0. Start: {}", chart.show(at.key))
            } else {
                format!(
                    "{step}. {}. Now: {}",
                    capitalize(&at.label),
                    chart.show(at.key)
                )
            });
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn rule_of(chart: &Chart, rule: &str) -> String {
    chart
        .charter
        .rules
        .get(rule)
        .cloned()
        .unwrap_or_else(|| format!("`{rule}`"))
}

/// Upstream's `summary.json`, in its key order.
pub(super) fn summary(chart: &Chart, flat: &Flat, regions: Option<&RegionsChart>) -> Json {
    let model = chart.charter.model;
    let drawn = regions.and_then(|r| r.drawn.as_ref().ok());
    let mut out = Map::new();
    out.insert("view".into(), chart.options.view.name().into());
    out.insert("modelStates".into(), model.states.len().into());
    out.insert(
        "modelTransitions".into(),
        model.steps.iter().filter(|s| s.from != s.to).count().into(),
    );
    out.insert(
        "chartStates".into(),
        drawn
            .map_or(flat.states.len(), |d| d.states[0] + d.states[1])
            .into(),
    );
    out.insert(
        "chartTransitions".into(),
        drawn.map_or(flat.transitions, |d| d.transitions).into(),
    );
    let status: Map<String, Json> = match drawn {
        Some(drawn) => drawn
            .status
            .iter()
            .map(|(status, count)| (status.name().to_owned(), (*count).into()))
            .collect(),
        None => FLAT_STATUS
            .iter()
            .map(|&status| {
                let count = flat
                    .states
                    .iter()
                    .filter(|&&k| chart.status(k) == status)
                    .count();
                (status.name().to_owned(), count.into())
            })
            .collect(),
    };
    out.insert("chartStatus".into(), status.into());
    if chart.charter.is_grouping() {
        let groups = drawn.map_or_else(|| chart.group_counts(&flat.drawn), |d| d.groups.clone());
        out.insert(
            "chartGroups".into(),
            groups
                .into_iter()
                .map(
                    |(name, color, states)| json!({"name": name, "color": color, "states": states}),
                )
                .collect(),
        );
        out.insert("mixedGroups".into(), chart.mixed_groups().into());
    }
    if let Some(regions) = regions {
        out.insert(
            "regions".into(),
            match &regions.drawn {
                Ok(drawn) => json!({
                    "drawn": true,
                    "states": drawn.states,
                    "transitions": drawn.transitions,
                    "all": drawn.all,
                    "flat": {"states": drawn.flat_states, "transitions": drawn.flat_transitions},
                }),
                Err(why) => json!({"drawn": false, "why": why}),
            },
        );
    }
    let classes: Vec<Json> = chart
        .classes
        .iter()
        .enumerate()
        .map(|(index, class)| {
            let trace = &model.traces[class.bug.trace];
            let mut entry = Map::new();
            entry.insert("n".into(), (index + 1).into());
            entry.insert("end".into(), chart.show(class.end()).into());
            entry.insert("invariant".into(), trace.rule.as_ref().into());
            entry.insert(
                "rule".into(),
                chart.charter.rules.get(trace.rule.as_ref()).cloned().into(),
            );
            entry.insert("cause".into(), class.bug.cause.clone().into());
            entry.insert("readerSteps".into(), (class.path.len() - 1).into());
            entry.insert("modelSteps".into(), trace.states.len().into());
            entry.insert("counterexamples".into(), class.bug.count.into());
            entry.insert(SECOND_KEY.into(), Json::Null);
            entry.into()
        })
        .collect();
    out.insert("classes".into(), classes.into());
    out.insert("codeLike".into(), code_like(chart, flat).into());
    out.into()
}

/// The state names, drawn labels and causes that read like code, which the reader would not see.
fn code_like(chart: &Chart, flat: &Flat) -> Vec<String> {
    let mut said: OrderedSet<String> = OrderedSet::default();
    if chart.options.view == View::Customer {
        said.extend(flat.states.iter().map(|&key| chart.name(key).to_owned()));
        said.extend(flat.labels.iter().cloned());
    }
    said.extend(
        chart
            .classes
            .iter()
            .filter_map(|class| class.bug.cause.clone())
            .filter(|cause| !cause.is_empty()),
    );
    said.iter()
        .filter(|text| CODE_LIKE.is_match(text))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("retryCount hit maxRetries" => true; "camel case")]
    #[test_case("Start(1)" => true; "a call")]
    #[test_case("sync_badge" => true; "snake case")]
    #[test_case("you start run 1" => false; "words")]
    #[test_case("Offline | Synced" => false; "a state")]
    fn reads_like_code(text: &str) -> bool {
        CODE_LIKE.is_match(text)
    }
}
