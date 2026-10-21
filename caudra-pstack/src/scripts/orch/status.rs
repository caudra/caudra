//! `status.md`: the rendered tables, the summary embedded in it, and what changed since the
//! summary the previous render left behind.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::compat::safe_integer;
use super::model::{Counts, Frontier, Gate, LedgerEntry, StatusReport, StatusSummary, Unit};

const FIRST_RENDER: &str = "first render";
const NO_CHANGES: &str = "no derived changes";
const EMPTY_TABLE: &str = "(none)";
const UNIT_COLUMNS: [&str; 7] = ["ID", "Track", "State", "Branch", "PR", "SHA", "Brief"];
const LEDGER_COLUMNS: [&str; 6] = ["PR", "SHA", "Verdict", "Evidence", "Verifier", "Timestamp"];
const FRONTIER_COLUMNS: [&str; 4] = ["Branch", "PR", "SHA", "State"];
const GATE_COLUMNS: [&str; 6] = ["ID", "Status", "Question", "Options", "Default", "Answer"];

static SUMMARY_COMMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<!-- orch-summary ([^\n\r\x{2028}\x{2029}]+) -->").expect("literal")
});

pub fn summarize(
    units: &[Unit],
    ledger: &[LedgerEntry],
    frontier: &Frontier,
    gates: &[Gate],
) -> StatusSummary {
    let mut open_gate_ids: Vec<String> = gates
        .iter()
        .filter(|gate| gate.answer.is_none())
        .map(|gate| gate.id.clone())
        .collect();
    open_gate_ids.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    StatusSummary {
        unit_states: Counts::of(units.iter().map(|unit| unit.state.as_str())),
        ledger_verdicts: Counts::of(ledger.iter().map(|row| row.verdict)),
        frontier_generation: frontier.generation as f64,
        open_gate_ids,
    }
}

fn count_record(value: &Value) -> Option<Counts> {
    value
        .as_object()?
        .iter()
        .map(|(name, count)| {
            let count = safe_integer(count).filter(|count| *count >= 0)?;
            Some((name.clone(), count.unsigned_abs()))
        })
        .collect::<Option<Vec<_>>>()
        .map(Counts)
}

/// The summary a previous render embedded, if it is there and well formed.
pub fn previous_summary(raw: &str) -> Option<StatusSummary> {
    let captures = SUMMARY_COMMENT.captures(raw)?;
    let value: Value = serde_json::from_str(captures.get(1)?.as_str()).ok()?;
    let object = value.as_object()?;
    let frontier_generation = object.get("frontierGeneration")?.as_f64()?;
    let open_gate_ids = object
        .get("openGateIds")?
        .as_array()?
        .iter()
        .map(|id| id.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some(StatusSummary {
        unit_states: count_record(object.get("unitStates")?)?,
        ledger_verdicts: count_record(object.get("ledgerVerdicts")?)?,
        frontier_generation,
        open_gate_ids,
    })
}

pub fn changed(before: Option<&StatusSummary>, after: &StatusSummary) -> String {
    let Some(before) = before else {
        return FIRST_RENDER.to_owned();
    };
    let mut result = Vec::new();
    let groups = [
        ("units", &before.unit_states, &after.unit_states),
        ("ledger", &before.ledger_verdicts, &after.ledger_verdicts),
    ];
    for (label, old_counts, new_counts) in groups {
        let mut names: Vec<&str> = old_counts
            .0
            .iter()
            .chain(&new_counts.0)
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        names.dedup();
        for name in names {
            let (old_count, new_count) = (old_counts.get(name), new_counts.get(name));
            if old_count != new_count {
                result.push(format!("{label} {name} {old_count}->{new_count}"));
            }
        }
    }
    if before.frontier_generation != after.frontier_generation {
        result.push(format!(
            "frontier generation {}->{}",
            before.frontier_generation, after.frontier_generation
        ));
    }
    if before.open_gate_ids != after.open_gate_ids {
        result.push(format!(
            "open gates {}->{}",
            before.open_gate_ids.len(),
            after.open_gate_ids.len()
        ));
    }
    if result.is_empty() {
        NO_CHANGES.to_owned()
    } else {
        result.join("; ")
    }
}

fn markdown(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

fn table<const N: usize>(headers: [&str; N], rows: &[[&str; N]]) -> String {
    if rows.is_empty() {
        return EMPTY_TABLE.to_owned();
    }
    let mut lines = vec![
        format!("| {} |", headers.join(" | ")),
        format!("| {} |", ["---"; N].join(" | ")),
    ];
    lines.extend(
        rows.iter()
            .map(|row| format!("| {} |", row.map(markdown).join(" | "))),
    );
    lines.join("\n")
}

/// The page `orch status` writes, with `summary_json` embedded for the next render to diff.
pub fn status_markdown(report: &StatusReport, generated: &str, summary_json: &str) -> String {
    let units: Vec<[&str; 7]> = report.units.iter().map(Unit::cells).collect();
    let ledger: Vec<[&str; 6]> = report.ledger.iter().map(LedgerEntry::cells).collect();
    let pr_numbers: Vec<String> = report
        .frontier
        .prs
        .iter()
        .map(|row| row.pr.to_string())
        .collect();
    let frontier: Vec<[&str; 4]> = report
        .frontier
        .prs
        .iter()
        .zip(&pr_numbers)
        .map(|(row, pr)| [row.branches.as_str(), pr, &row.sha, row.state.as_str()])
        .collect();
    let gates: Vec<[&str; 6]> = report
        .gates
        .iter()
        .map(|gate| {
            [
                gate.id.as_str(),
                gate.kind(),
                &gate.question,
                &gate.options,
                &gate.default_answer,
                gate.answer.as_deref().unwrap_or_default(),
            ]
        })
        .collect();
    let lowest_unmerged = report
        .frontier
        .lowest_unmerged
        .map_or_else(|| "none".to_owned(), |pr| pr.to_string());
    format!(
        "# Orchestrate status

Generated: {generated}

## Units

States: {unit_states}

{unit_table}

## Verification ledger

Verdicts: {ledger_verdicts}

{ledger_table}

## Frontier

Generation: {generation}
Lowest unmerged: {lowest_unmerged}

{frontier_table}

## Gates

{gate_table}

<!-- orch-summary {summary_json} -->
",
        unit_states = report.summary.unit_states.line(),
        unit_table = table(UNIT_COLUMNS, &units),
        ledger_verdicts = report.summary.ledger_verdicts.line(),
        ledger_table = table(LEDGER_COLUMNS, &ledger),
        generation = report.frontier.generation,
        frontier_table = table(FRONTIER_COLUMNS, &frontier),
        gate_table = table(GATE_COLUMNS, &gates),
    )
}
