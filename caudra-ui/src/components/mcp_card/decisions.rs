//! The card of a snippet's `decide()` or `rerank()`: what was judged and each question's verdict
//! with a bar per option, or the candidates the rerank kept, best first, each under the label it
//! was given. Anything that does not read as such an answer is left to the generic card.

use caudra_agent::{DecisionHelper, McpBlock, McpHelper, McpOutput};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::{Rows, one_line};
use crate::components::decision_view::{
    self, SUMMARY_SEPARATOR, Verdict, bar_spans, bar_width, from_script, pct,
};
use crate::components::list_picker::truncate_label;
use crate::theme::{self, Theme};

const JUDGED_LABEL: &str = "Judged: ";
const MODEL_LABEL: &str = "model ";
const QUERY_LABEL: &str = "Query: ";
const KEPT_PREFIX: &str = "Kept ";
const KEPT_OF: &str = " of ";
const THRESHOLD_PREFIX: &str = "threshold ";
const TOP_PREFIX: &str = "top ";
const NOT_KEPT_SUFFIX: &str = " not kept";
const INDENT: &str = "  ";
const GAP: &str = "  ";
const RERANK_BAR_CELLS: usize = 12;
/// A candidate this likely to be relevant has its bar drawn bright.
const RELEVANT_AT: f64 = 0.5;
const INPUT_FIELD: &str = "input";
const MODEL_FIELD: &str = "model";
const QUERY_FIELD: &str = "query";
const CANDIDATES_FIELD: &str = "candidates";
const THRESHOLD_FIELD: &str = "threshold";
const TOP_K_FIELD: &str = "top_k";
const INDEX_FIELD: &str = "index";
const SCORE_FIELD: &str = "score";
/// The fields a candidate object is named by, in the order they are looked for.
const LABEL_FIELDS: &[&str] = &["name", "title", "label", "id", "path", "uri"];
/// The fields that say more about a candidate, beside its label.
const DETAIL_FIELDS: &[&str] = &["description", "summary", "text"];

/// One kept candidate: where it sat in the input and how relevant it was judged.
struct Ranked {
    index: usize,
    score: f64,
}

pub(super) fn render(
    helper: DecisionHelper,
    arguments: Option<&Value>,
    output: &McpOutput,
    width: u16,
) -> Option<Rows> {
    let answer = answer(output)?;
    let width = usize::from(width);
    let theme = theme::current();
    let lines = match helper {
        DecisionHelper::Decide => decide_rows(arguments, &answer, width, &theme)?,
        DecisionHelper::Rerank => rerank_rows(arguments, &answer, width, &theme)?,
    };
    let mut rows = Rows::default();
    rows.push(lines);
    Some(rows)
}

/// What a settled row's line says in place of its arguments: each verdict, or how many the rerank
/// kept and the best of them.
pub(crate) fn summary(
    helper: DecisionHelper,
    arguments: Option<&Value>,
    output: &McpOutput,
) -> Option<String> {
    let answer = answer(output)?;
    match helper {
        DecisionHelper::Decide => Some(decision_view::summary(&from_script(arguments, &answer)?)),
        DecisionHelper::Rerank => {
            let ranked = ranked(&answer)?;
            let total = candidates(arguments).map_or(ranked.len(), <[Value]>::len);
            let kept = format!("{KEPT_PREFIX}{}{KEPT_OF}{total}", ranked.len());
            Some(match ranked.first() {
                Some(best) => {
                    let (label, _) = candidate_text(candidates(arguments)?.get(best.index)?);
                    format!(
                        "{kept}{SUMMARY_SEPARATOR}{} {}",
                        one_line(&label),
                        pct(best.score)
                    )
                }
                None => kept,
            })
        }
    }
}

/// The answer the snippet received, which is what a selection over the card copies: the bars stand
/// for it rather than spell any of it.
pub(crate) fn copied(output: &McpOutput) -> Option<String> {
    let Some(McpHelper::Decision(_)) = output.helper else {
        return None;
    };
    serde_json::to_string_pretty(&answer(output)?).ok()
}

fn answer(output: &McpOutput) -> Option<Value> {
    match output.blocks.as_slice() {
        [McpBlock::Text { text }] => serde_json::from_str(text).ok(),
        _ => None,
    }
}

fn decide_rows(
    arguments: Option<&Value>,
    answer: &Value,
    width: usize,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let verdicts: Vec<Verdict> = from_script(arguments, answer)?;
    let mut lines = Vec::new();
    if let Some(input) = arguments.and_then(|arguments| arguments.get(INPUT_FIELD)) {
        lines.push(labelled(JUDGED_LABEL, &value_text(input), width, theme));
        lines.push(Line::default());
    }
    lines.extend(decision_view::rows(&verdicts, width, theme));
    if let Some(model) = answer.get(MODEL_FIELD).and_then(Value::as_str) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{MODEL_LABEL}{}", one_line(model)),
            theme.tool_dim,
        )));
    }
    Some(lines)
}

fn rerank_rows(
    arguments: Option<&Value>,
    answer: &Value,
    width: usize,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let ranked = ranked(answer)?;
    let candidates = candidates(arguments)?;
    let mut lines = Vec::new();
    if let Some(query) = arguments.and_then(|arguments| arguments.get(QUERY_FIELD)) {
        lines.push(labelled(QUERY_LABEL, &value_text(query), width, theme));
    }
    let mut kept = vec![Span::raw(format!(
        "{KEPT_PREFIX}{}{KEPT_OF}{}",
        ranked.len(),
        candidates.len()
    ))];
    let given = |field| arguments?.get(field).filter(|value| !value.is_null());
    if let Some(threshold) = given(THRESHOLD_FIELD).and_then(Value::as_f64) {
        kept.push(Span::styled(
            format!("{SUMMARY_SEPARATOR}{THRESHOLD_PREFIX}{threshold:.2}"),
            theme.tool_dim,
        ));
    }
    if let Some(top_k) = given(TOP_K_FIELD).and_then(Value::as_u64) {
        kept.push(Span::styled(
            format!("{SUMMARY_SEPARATOR}{TOP_PREFIX}{top_k}"),
            theme.tool_dim,
        ));
    }
    lines.push(Line::from(kept));
    let rank_cols = ranked.len().to_string().len();
    let prefix_cols =
        INDENT.len() + rank_cols + GAP.len() + bar_width(RERANK_BAR_CELLS) + GAP.len();
    let room = width.saturating_sub(prefix_cols).max(1);
    for (rank, Ranked { index, score }) in ranked.iter().enumerate() {
        let (label, detail) = candidate_text(candidates.get(*index)?);
        let label = truncate_label(&one_line(&label), room);
        let mut spans = vec![
            Span::raw(INDENT),
            Span::styled(format!("{:>rank_cols$}", rank + 1), theme.tool_dim),
            Span::raw(GAP),
        ];
        spans.extend(bar_spans(
            *score,
            RERANK_BAR_CELLS,
            *score >= RELEVANT_AT,
            theme,
        ));
        spans.push(Span::raw(GAP));
        let left = room.saturating_sub(label.width() + GAP.len());
        spans.push(Span::styled(label, theme.bold));
        if let Some(detail) = detail.filter(|_| left > 1) {
            spans.push(Span::raw(GAP));
            spans.push(Span::styled(
                truncate_label(&one_line(&detail), left),
                theme.tool_dim,
            ));
        }
        lines.push(Line::from(spans));
    }
    let dropped = candidates.len().saturating_sub(ranked.len());
    if dropped > 0 {
        lines.push(Line::from(Span::styled(
            format!("{INDENT}{dropped}{NOT_KEPT_SUFFIX}"),
            theme.tool_dim,
        )));
    }
    Some(lines)
}

fn ranked(answer: &Value) -> Option<Vec<Ranked>> {
    answer
        .as_array()?
        .iter()
        .map(|entry| {
            Some(Ranked {
                index: usize::try_from(entry.get(INDEX_FIELD)?.as_u64()?).ok()?,
                score: entry
                    .get(SCORE_FIELD)?
                    .as_f64()
                    .filter(|score| (0.0..=1.0).contains(score))?,
            })
        })
        .collect()
}

fn candidates(arguments: Option<&Value>) -> Option<&[Value]> {
    arguments?
        .get(CANDIDATES_FIELD)?
        .as_array()
        .map(Vec::as_slice)
}

/// A candidate's label and what else it says: a string as written, an object by its naming field
/// and its describing one, and anything else as compact JSON.
fn candidate_text(candidate: &Value) -> (String, Option<String>) {
    let Some(fields) = candidate.as_object() else {
        return (value_text(candidate), None);
    };
    let first = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| fields.get(*name).filter(|value| !value.is_null()))
            .map(value_text)
    };
    match first(LABEL_FIELDS) {
        Some(label) => (label, first(DETAIL_FIELDS)),
        None => (candidate.to_string(), None),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}

fn labelled(label: &'static str, text: &str, width: usize, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme.accent),
        Span::raw(truncate_label(
            &one_line(text),
            width.saturating_sub(label.len()).max(1),
        )),
    ])
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const QUERY: &str = "tools that add numbers";

    fn output(helper: DecisionHelper, answer: &Value) -> McpOutput {
        McpOutput {
            blocks: vec![McpBlock::Text {
                text: serde_json::to_string_pretty(answer).unwrap(),
            }],
            helper: Some(McpHelper::Decision(helper)),
            ..McpOutput::default()
        }
    }

    fn rerank() -> (Value, Value) {
        (
            json!({
                "query": QUERY,
                "candidates": ["subtract", {"name": "get_sum", "description": "Adds two numbers"}, {"x": 1}],
                "top_k": 2,
            }),
            json!([{"index": 1, "score": 0.62}, {"index": 2, "score": 0.314}]),
        )
    }

    fn drawn(rows: &Rows) -> Vec<String> {
        rows.lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_rerank_lists_what_it_kept_best_first_by_label() {
        let (arguments, answer) = rerank();
        let rows = render(
            DecisionHelper::Rerank,
            Some(&arguments),
            &output(DecisionHelper::Rerank, &answer),
            80,
        )
        .unwrap();
        let drawn = drawn(&rows);
        assert_eq!(drawn[0], format!("{QUERY_LABEL}{QUERY}"));
        assert_eq!(drawn[1], "Kept 2 of 3 \u{b7} top 2");
        assert!(
            drawn[2].contains("62.0%  get_sum  Adds two numbers"),
            "{drawn:?}"
        );
        assert!(drawn[3].contains("31.4%  {\"x\":1}"), "{drawn:?}");
        assert_eq!(drawn[4], format!("{INDENT}1{NOT_KEPT_SUFFIX}"));
        assert!(drawn.iter().all(|line| line.width() <= 80), "{drawn:?}");
    }

    #[test]
    fn a_rerank_summary_names_its_best() {
        let (arguments, answer) = rerank();
        assert_eq!(
            summary(
                DecisionHelper::Rerank,
                Some(&arguments),
                &output(DecisionHelper::Rerank, &answer)
            )
            .as_deref(),
            Some("Kept 2 of 3 \u{b7} get_sum 62.0%")
        );
    }

    #[test]
    fn a_decide_card_shows_what_was_judged_and_the_model() {
        let arguments = json!({
            "input": "The parser crashes.",
            "questions": [{"type": "predicate", "name": "parser_bug", "instructions": "A parser bug?"}],
        });
        let answer = json!({"model": "m1", "answers": [{"type": "predicate", "name": "parser_bug", "probability": 0.9}]});
        let rows = render(
            DecisionHelper::Decide,
            Some(&arguments),
            &output(DecisionHelper::Decide, &answer),
            80,
        )
        .unwrap();
        let drawn = drawn(&rows);
        assert_eq!(drawn[0], format!("{JUDGED_LABEL}The parser crashes."));
        assert!(drawn[2].starts_with("parser_bug"), "{drawn:?}");
        assert_eq!(drawn.last().unwrap(), &format!("{MODEL_LABEL}m1"));
    }

    #[test_case(DecisionHelper::Rerank, json!([{"index": 9, "score": 0.5}]) ; "index_past_the_candidates")]
    #[test_case(DecisionHelper::Rerank, json!({"answers": []}) ; "not_a_list")]
    #[test_case(DecisionHelper::Decide, json!([{"index": 0, "score": 0.5}]) ; "a_rerank_as_decide")]
    fn an_answer_it_cannot_read_is_left_to_the_generic_card(helper: DecisionHelper, answer: Value) {
        let (arguments, _) = rerank();
        assert!(render(helper, Some(&arguments), &output(helper, &answer), 80).is_none());
    }
}
