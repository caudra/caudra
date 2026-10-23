//! What upstream's `tlc-to-chart.mjs` writes beside a chart: one reader story per bug, and the
//! counts of `summary.json`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value as Json, json};

use super::chart::{Chart, Class, Flat, Status, View};
use super::ordered::OrderedSet;
use super::regions::RegionsChart;
use super::second::{Replay, Role, STARTS_ELSEWHERE};
use super::text::capitalize;

const ONLY_WAY: &str = "This is the only way to get here in the model.";
/// The key upstream names a bug's replay on the second model under when there is none.
const NO_SECOND_KEY: &str = "fix";
const SECOND: &str = "second";
const LISTED: &str = "listed";
const FIELD_SEPARATOR: &str = ".";
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
    let role = chart.charter.role();
    let mut lines: Vec<String> = Vec::new();
    for (index, class) in chart.classes.iter().enumerate() {
        let trace = chart.charter.trace(class.bug);
        let header = match role {
            Some(role) if class.bug.introduced => introduced_header(role),
            _ => String::new(),
        };
        lines.push(format!(
            "## {}. {header}{}",
            index + 1,
            chart.show(class.end())
        ));
        lines.push(String::new());
        lines.push(format!("Rule broken: {}", rule_of(chart, &trace.rule)));
        if let Some(cause) = class.bug.cause.as_deref().filter(|cause| !cause.is_empty()) {
            lines.push(format!(
                "Why: {}.",
                cause.strip_suffix('.').unwrap_or(cause)
            ));
        }
        if let Some(listed) = role.and_then(Role::listed) {
            lines.push(listed.line(&trace.rule));
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
        if let Some(said) = role.and_then(|role| second_line(chart, class, role)) {
            lines.push(said);
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn introduced_header(role: &Role) -> String {
    if role.is_fix() {
        format!("New with {}: ", role.label())
    } else {
        format!("Only in {}: ", role.label())
    }
}

/// What the second model does with the bug's steps, as upstream's `secondLine`.
fn second_line(chart: &Chart, class: &Class, role: &Role) -> Option<String> {
    let (label, within) = (role.label(), role.within());
    let within_cap = capitalize(within);
    if class.bug.introduced {
        return Some(format!(
            "Only {label} has this bug. The steps above happen {within} {label}."
        ));
    }
    let same = "you see the same screens and steps";
    match class.bug.replay.as_ref()? {
        Replay::StartsElsewhere => Some(format!(
            "{} cannot take these steps: it {STARTS_ELSEWHERE}.",
            capitalize(label)
        )),
        Replay::Same { checked: false, .. } => Some(format!(
            "{within_cap} {label} {same}, but {label} does not check this rule."
        )),
        Replay::Same {
            rule_holds: true, ..
        } => Some(format!(
            "{within_cap} {label} {same}, yet the rule holds, so the state labels do not show what {label} changes."
        )),
        Replay::Same { .. } => Some(format!(
            "{within_cap} {label} {same}, so {label} does not change this path."
        )),
        Replay::Differs(turn) => {
            let divergence = class.divergence.as_ref()?;
            let outcome = if divergence.disabled {
                "cannot happen, so you stay at"
            } else if divergence.key == divergence.from {
                "keeps you at"
            } else {
                "takes you to"
            };
            let at = chart.show(divergence.key);
            Some(if turn.hidden {
                let when = if turn.step == 0 {
                    "before your first step".to_owned()
                } else {
                    format!("after step {}", turn.step)
                };
                format!(
                    "{within_cap} {label}, {when}, \"{}\" happens and {outcome}: {at}.",
                    turn.said
                )
            } else {
                format!(
                    "{within_cap} {label}, step {} ({}) {outcome}: {at}.",
                    turn.step, turn.said
                )
            })
        }
    }
}

/// Upstream's `secondOf`: the bug on the second model, in the summary's words.
fn second_of(chart: &Chart, class: &Class, role: &Role) -> Json {
    if class.bug.introduced {
        let place = if role.is_fix() { "new with" } else { "only in" };
        return format!("{place} {}", role.label()).into();
    }
    match class.bug.replay.as_ref() {
        None => Json::Null,
        Some(Replay::Same { checked: false, .. }) => "same screens, rule not checked".into(),
        Some(Replay::Same {
            rule_holds: true, ..
        }) => "same screens, but the rule holds".into(),
        Some(Replay::Same { .. }) => "unchanged".into(),
        Some(Replay::StartsElsewhere) => STARTS_ELSEWHERE.into(),
        Some(Replay::Differs(_)) => class
            .divergence
            .as_ref()
            .map_or(Json::Null, |divergence| chart.show(divergence.key).into()),
    }
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
    let role = chart.charter.role();
    if let (Some(role), Some(second)) = (role, &chart.charter.second) {
        let mut entry = Map::new();
        entry.insert("role".into(), role.name().into());
        entry.insert("label".into(), role.label().into());
        let lacks: Vec<String> = second
            .lacks
            .iter()
            .map(|path| path.join(FIELD_SEPARATOR))
            .collect();
        entry.insert("lacks".into(), lacks.into());
        if let Some(listed) = role.listed() {
            entry.insert("listed".into(), listed.json());
        }
        out.insert(SECOND.into(), entry.into());
    }
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
            let trace = chart.charter.trace(class.bug);
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
            let second = role.map_or(Json::Null, |role| second_of(chart, class, role));
            entry.insert(role.map_or(NO_SECOND_KEY, Role::name).into(), second);
            if let Some(listed) = role.and_then(Role::listed) {
                entry.insert(LISTED.into(), listed.configs(&trace.rule).into());
            }
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
