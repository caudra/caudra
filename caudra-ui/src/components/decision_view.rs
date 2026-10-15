//! Decision answers drawn the way the ninfer playground draws them: each question under its name
//! and type with the winning option and its probability, then a bar per option, the winner bright
//! and the rest dim, and a warning when the winner falls short of sure. Reads both the answer a
//! snippet or workflow receives and the internal answer the decision log keeps.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};
use unicode_width::UnicodeWidthStr;

use super::gauge::{self, CELL_EIGHTHS};
use super::list_picker::truncate_label;
use super::mcp_card::one_line;
use crate::theme::Theme;

/// A winner less likely than this reads as uncertain, as in the playground.
const UNSURE: f64 = 0.6;
const LOW_CLAMP: f64 = 0.001;
const HIGH_CLAMP: f64 = 0.999;
const PERCENT: f64 = 100.0;
const BELOW_LOW: &str = "<0.1%";
const ABOVE_HIGH: &str = ">99.9%";
const PERCENT_COLS: usize = ABOVE_HIGH.len();
const PREDICATE_NO: &str = "no";
const PREDICATE_YES: &str = "yes";
const PREDICATE_TAG: &str = "PREDICATE";
const CHOICE_TAG: &str = "CHOICE";
const SCORE_TAG: &str = "SCORE";
const UNCERTAIN_PREFIX: &str = "uncertain, runner-up ";
const SCORE_PREFIX: &str = "score ";
const SCORE_RANGE: &str = " on 0\u{2013}";
const CONFIDENCE_PREFIX: &str = "confidence ";
const INDENT: &str = "  ";
const GAP: &str = "  ";
const SPACE: &str = " ";
const DETAIL_SEPARATOR: &str = " \u{b7} ";
pub(crate) const SUMMARY_SEPARATOR: &str = " \u{b7} ";
const LABEL_SHARE_PERCENT: usize = 40;
const MIN_BAR_CELLS: usize = 6;
const MAX_BAR_CELLS: usize = 24;
const SCALE_START: char = '\u{251c}';
const SCALE_END: char = '\u{2524}';
const SCALE_LINE: char = '\u{2500}';
const SCALE_TICK: char = '\u{253c}';
const SCALE_MARK: char = '\u{25cf}';

const TYPE_FIELD: &str = "type";
const NAME_FIELD: &str = "name";
const ANSWERS_FIELD: &str = "answers";
const QUESTIONS_FIELD: &str = "questions";
const INSTRUCTIONS_FIELD: &str = "instructions";
const PROBABILITY_FIELD: &str = "probability";
const PROBABILITIES_FIELD: &str = "probabilities";
const CONFIDENCE_FIELD: &str = "confidence";
const VALUE_FIELD: &str = "value";
const LABEL_FIELD: &str = "label";
const DESCRIPTION_FIELD: &str = "description";
const CHOICES_FIELD: &str = "choices";
const LEVELS_FIELD: &str = "levels";
const SCORE_FIELD: &str = "score";
const CRITERIA_FIELD: &str = "criteria";
const LEGEND_FIELD: &str = "legend";
const NOUL_FIELD: &str = "noul";
const STATEMENT_FIELD: &str = "statement";
const CANDIDATE_FIELD: &str = "candidate";
const TRUE_KEY: &str = "true";
const FALSE_KEY: &str = "false";
const PREDICATE_TYPE: &str = "predicate";
const CHOICE_TYPE: &str = "choice";
const SCORE_TYPE: &str = "score";
const NOUL_TYPE: &str = "noul";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Predicate,
    Choice,
    Score,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Self::Predicate => PREDICATE_TAG,
            Self::Choice => CHOICE_TAG,
            Self::Score => SCORE_TAG,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Outcome {
    label: String,
    description: Option<String>,
    probability: f64,
}

/// One answered question, whichever shape it came in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verdict {
    name: String,
    kind: Kind,
    instructions: Option<String>,
    options: Vec<Outcome>,
    score: Option<f64>,
    confidence: Option<f64>,
}

impl Verdict {
    fn predicate(name: String, instructions: Option<String>, probability: f64) -> Self {
        Self::predicate_described(name, instructions, probability, [None, None])
    }

    fn predicate_described(
        name: String,
        instructions: Option<String>,
        probability: f64,
        [no, yes]: [Option<String>; 2],
    ) -> Self {
        Self {
            name,
            kind: Kind::Predicate,
            instructions,
            options: vec![
                Outcome {
                    label: PREDICATE_NO.to_owned(),
                    description: no,
                    probability: 1.0 - probability,
                },
                Outcome {
                    label: PREDICATE_YES.to_owned(),
                    description: yes,
                    probability,
                },
            ],
            score: None,
            confidence: None,
        }
    }

    /// The first of the likeliest options, so a tie keeps the declared order.
    fn winner(&self) -> Option<usize> {
        self.best_except(None)
    }

    fn runner_up(&self) -> Option<usize> {
        self.best_except(self.winner())
    }

    fn best_except(&self, skipped: Option<usize>) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, option) in self.options.iter().enumerate() {
            if Some(index) != skipped
                && best.is_none_or(|best| option.probability > self.options[best].probability)
            {
                best = Some(index);
            }
        }
        best
    }

    fn uncertain(&self) -> bool {
        self.winner()
            .is_some_and(|winner| self.options[winner].probability < UNSURE)
    }

    fn option_label(&self, index: usize) -> String {
        let option = &self.options[index];
        match self.kind {
            Kind::Score => format!("{index} {}", option.label),
            Kind::Predicate | Kind::Choice => option.label.clone(),
        }
    }

    /// `name winner pct`, the verdict a folded row shows.
    fn short(&self) -> String {
        match self.winner() {
            Some(winner) => format!(
                "{} {} {}",
                self.name,
                self.options[winner].label,
                pct(self.options[winner].probability)
            ),
            None => self.name.clone(),
        }
    }
}

/// A probability as the playground prints it: one decimal, and never a rounded 0 or 100 for
/// what is only nearly so.
pub(crate) fn pct(probability: f64) -> String {
    if probability > 0.0 && probability < LOW_CLAMP {
        BELOW_LOW.to_owned()
    } else if probability > HIGH_CLAMP && probability < 1.0 {
        ABOVE_HIGH.to_owned()
    } else {
        format!("{:.1}%", probability * PERCENT)
    }
}

/// `probability` of `cells` filled to the nearest eighth, and the track left after it.
pub(crate) fn bar(probability: f64, cells: usize) -> (String, String) {
    let eighths = (probability.clamp(0.0, 1.0) * (cells * CELL_EIGHTHS) as f64).round() as usize;
    let (fill, used) = gauge::fill(eighths.min(cells * CELL_EIGHTHS));
    (fill, gauge::track(cells.saturating_sub(used)))
}

/// A bar and its percentage, bright for the option that won and dim otherwise.
pub(crate) fn bar_spans(
    probability: f64,
    cells: usize,
    won: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let (fill, track) = bar(probability, cells);
    let style = match won {
        true => theme.accent,
        false => theme.tool_dim,
    };
    vec![
        Span::styled(fill, style),
        Span::styled(track, theme.tool_dim),
        Span::raw(SPACE),
        Span::styled(format!("{:>PERCENT_COLS$}", pct(probability)), style),
    ]
}

/// The columns [`bar_spans`] takes for a bar of `cells`.
pub(crate) fn bar_width(cells: usize) -> usize {
    cells + SPACE.len() + PERCENT_COLS
}

/// Cells for a bar beside a label column `label_cols` wide, inside `width`.
fn bar_cells(width: usize, label_cols: usize) -> usize {
    width
        .saturating_sub(INDENT.len() + label_cols + GAP.len() + bar_width(0))
        .clamp(MIN_BAR_CELLS, MAX_BAR_CELLS)
}

/// The label column: as wide as the widest label, but no more than its share of `width`.
fn label_cols<'a>(labels: impl IntoIterator<Item = &'a str>, width: usize) -> usize {
    labels
        .into_iter()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or_default()
        .min(width * LABEL_SHARE_PERCENT / 100)
        .max(1)
}

/// `label` cut or padded to exactly `cols`.
fn padded(label: &str, cols: usize) -> String {
    let cut = truncate_label(label, cols);
    let pad = cols.saturating_sub(cut.width());
    format!("{cut}{}", SPACE.repeat(pad))
}

/// The verdicts of a snippet's or workflow's `decide()` answer, the question each came from found
/// by name in `arguments`, or `None` for any shape it does not know.
pub(crate) fn from_script(arguments: Option<&Value>, answer: &Value) -> Option<Vec<Verdict>> {
    let questions = arguments
        .and_then(|arguments| arguments.get(QUESTIONS_FIELD))
        .and_then(Value::as_array);
    let verdicts: Vec<Verdict> = answer
        .get(ANSWERS_FIELD)?
        .as_array()?
        .iter()
        .map(|answer| {
            let name = answer.get(NAME_FIELD)?.as_str()?;
            let question = questions.and_then(|questions| {
                questions
                    .iter()
                    .find(|question| question.get(NAME_FIELD).and_then(Value::as_str) == Some(name))
            });
            let instructions = question
                .and_then(|question| question.get(INSTRUCTIONS_FIELD))
                .and_then(content_text);
            let described = |field: &str, index: usize| {
                question?
                    .get(field)?
                    .get(index)?
                    .get(DESCRIPTION_FIELD)
                    .and_then(content_text)
            };
            let options = |field: &str| -> Option<Vec<Outcome>> {
                answer
                    .get(PROBABILITIES_FIELD)?
                    .as_array()?
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let label = entry.get(LABEL_FIELD).or_else(|| entry.get(VALUE_FIELD))?;
                        Some(Outcome {
                            label: content_text(label)?,
                            description: described(field, index),
                            probability: probability(entry.get(PROBABILITY_FIELD)?)?,
                        })
                    })
                    .collect()
            };
            let name = name.to_owned();
            let confidence = answer.get(CONFIDENCE_FIELD).and_then(Value::as_f64);
            match answer.get(TYPE_FIELD)?.as_str()? {
                PREDICATE_TYPE => Some(Verdict::predicate(
                    name,
                    instructions,
                    probability(answer.get(PROBABILITY_FIELD)?)?,
                )),
                CHOICE_TYPE => Some(Verdict {
                    name,
                    kind: Kind::Choice,
                    instructions,
                    options: options(CHOICES_FIELD)?,
                    score: None,
                    confidence,
                }),
                SCORE_TYPE => Some(Verdict {
                    name,
                    kind: Kind::Score,
                    instructions,
                    options: options(LEVELS_FIELD)?,
                    score: Some(answer.get(SCORE_FIELD)?.as_f64()?),
                    confidence,
                }),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    (!verdicts.is_empty()).then_some(verdicts)
}

/// The verdicts of a logged decision: questions and answers keyed by question id, in the engine's
/// own shape. A rerank batch's question is named by the candidate it judged.
pub(crate) fn from_logged(questions: &Value, answers: &Value) -> Option<Vec<Verdict>> {
    let answers = answers.as_object()?;
    let verdicts: Vec<Verdict> = questions
        .as_object()?
        .iter()
        .map(|(id, question)| {
            let answer = answers.get(id)?;
            let instructions = question.get(INSTRUCTIONS_FIELD);
            let (name, instructions) =
                match instructions.and_then(|instructions| instructions.get(CANDIDATE_FIELD)) {
                    Some(candidate) => (
                        content_text(candidate)?,
                        instructions
                            .and_then(|instructions| instructions.get(STATEMENT_FIELD))
                            .and_then(content_text),
                    ),
                    None => (id.clone(), instructions.and_then(content_text)),
                };
            let criteria = question.get(CRITERIA_FIELD);
            let probabilities = || answer.get(PROBABILITIES_FIELD)?.as_object();
            let confidence = answer.get(CONFIDENCE_FIELD).and_then(Value::as_f64);
            match answer.get(TYPE_FIELD)?.as_str()? {
                NOUL_TYPE => {
                    let described = |key| criteria?.get(key).and_then(content_text);
                    Some(Verdict::predicate_described(
                        name,
                        instructions,
                        probability(answer.get(NOUL_FIELD)?)?,
                        [described(FALSE_KEY), described(TRUE_KEY)],
                    ))
                }
                CHOICE_TYPE => {
                    let probabilities = probabilities()?;
                    let declared = criteria.and_then(Value::as_object);
                    let keys: Vec<&String> = match declared {
                        Some(declared) => declared.keys().collect(),
                        None => probabilities.keys().collect(),
                    };
                    let options = keys
                        .into_iter()
                        .map(|key| {
                            Some(Outcome {
                                label: key.clone(),
                                description: declared
                                    .and_then(|declared| declared.get(key))
                                    .and_then(content_text),
                                probability: probability(probabilities.get(key)?)?,
                            })
                        })
                        .collect::<Option<_>>()?;
                    Some(Verdict {
                        name,
                        kind: Kind::Choice,
                        instructions,
                        options,
                        score: None,
                        confidence,
                    })
                }
                SCORE_TYPE => {
                    let probabilities = probabilities()?;
                    let legend = answer.get(LEGEND_FIELD).and_then(Value::as_object);
                    let levels = criteria.and_then(Value::as_array);
                    let count = legend.map_or(probabilities.len(), Map::len);
                    let options = (0..count)
                        .map(|level| {
                            let key = level.to_string();
                            let label = legend
                                .and_then(|legend| legend.get(&key))
                                .and_then(content_text)
                                .unwrap_or_else(|| key.clone());
                            let description = levels
                                .and_then(|levels| levels.get(level))
                                .and_then(content_text)
                                .filter(|description| *description != label);
                            Some(Outcome {
                                label,
                                description,
                                probability: probability(probabilities.get(&key)?)?,
                            })
                        })
                        .collect::<Option<_>>()?;
                    Some(Verdict {
                        name,
                        kind: Kind::Score,
                        instructions,
                        options,
                        score: Some(answer.get(SCORE_FIELD)?.as_f64()?),
                        confidence,
                    })
                }
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    (!verdicts.is_empty()).then_some(verdicts)
}

/// Each verdict's winner, joined for a row too narrow for bars.
pub(crate) fn summary(verdicts: &[Verdict]) -> String {
    verdicts
        .iter()
        .map(Verdict::short)
        .collect::<Vec<_>>()
        .join(SUMMARY_SEPARATOR)
}

/// The verdicts as lines `width` columns wide, a blank line between questions.
pub(crate) fn rows(verdicts: &[Verdict], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for verdict in verdicts {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(verdict_rows(verdict, width, theme));
    }
    lines
}

fn verdict_rows(verdict: &Verdict, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let winner = verdict.winner();
    let uncertain = verdict.uncertain();
    let mut lines = vec![headline(verdict, winner, uncertain, width, theme)];
    if let Some(instructions) = &verdict.instructions {
        let text = one_line(instructions);
        lines.push(Line::from(Span::styled(
            truncate_label(&text, width.saturating_sub(INDENT.len()).max(1)),
            theme.tool_dim,
        )));
    }
    let mut details: Vec<Span<'static>> = Vec::new();
    if let (Some(score), Kind::Score) = (verdict.score, verdict.kind) {
        details.push(Span::styled(
            format!(
                "{SCORE_PREFIX}{score:.2}{SCORE_RANGE}{}",
                verdict.options.len().saturating_sub(1)
            ),
            theme.tool_dim,
        ));
    }
    if uncertain && let Some(runner_up) = verdict.runner_up() {
        if !details.is_empty() {
            details.push(Span::styled(DETAIL_SEPARATOR, theme.tool_dim));
        }
        details.push(Span::styled(
            format!(
                "{UNCERTAIN_PREFIX}{} {}",
                verdict.option_label(runner_up),
                pct(verdict.options[runner_up].probability)
            ),
            theme.tool_warning,
        ));
    }
    if !details.is_empty() {
        lines.push(Line::from(details));
    }
    let labels: Vec<String> = (0..verdict.options.len())
        .map(|index| one_line(&verdict.option_label(index)))
        .collect();
    let label_cols = label_cols(labels.iter().map(String::as_str), width);
    let cells = bar_cells(width, label_cols);
    if let (Some(score), Kind::Score) = (verdict.score, verdict.kind) {
        lines.push(scale(
            score,
            verdict.options.len(),
            label_cols,
            cells,
            theme,
        ));
    }
    let bar_column = INDENT.len() + label_cols + GAP.len();
    for (index, (option, label)) in verdict.options.iter().zip(labels).enumerate() {
        let won = Some(index) == winner;
        let label_style = match won {
            true => theme.accent.add_modifier(Modifier::BOLD),
            false => Style::default(),
        };
        let mut spans = vec![
            Span::raw(INDENT),
            Span::styled(padded(&label, label_cols), label_style),
            Span::raw(GAP),
        ];
        spans.extend(bar_spans(option.probability, cells, won, theme));
        lines.push(Line::from(spans));
        if let Some(description) = &option.description {
            lines.push(Line::from(vec![
                Span::raw(SPACE.repeat(bar_column)),
                Span::styled(
                    truncate_label(
                        &one_line(description),
                        width.saturating_sub(bar_column).max(1),
                    ),
                    theme.tool_dim,
                ),
            ]));
        }
    }
    if let Some(confidence) = verdict.confidence {
        lines.push(Line::from(Span::styled(
            format!("{INDENT}{CONFIDENCE_PREFIX}{confidence:.4}"),
            theme.tool_dim,
        )));
    }
    lines
}

/// `name  TYPE` on the left, the winner and its probability on the right, in the warning style
/// when the winner is uncertain.
fn headline(
    verdict: &Verdict,
    winner: Option<usize>,
    uncertain: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let name = one_line(&verdict.name);
    let tag = verdict.kind.tag();
    let verdict_text = winner.map(|winner| {
        format!(
            "{} {}",
            one_line(&verdict.option_label(winner)),
            pct(verdict.options[winner].probability)
        )
    });
    let right_cols = verdict_text.as_deref().map_or(0, UnicodeWidthStr::width);
    let left_room = width
        .saturating_sub(right_cols + GAP.len() * 2 + tag.len())
        .max(1);
    let name = truncate_label(&name, left_room);
    let used = name.width() + GAP.len() + tag.len();
    let mut spans = vec![
        Span::styled(name, theme.bold),
        Span::raw(GAP),
        Span::styled(tag, theme.tool_dim),
    ];
    if let Some(text) = verdict_text {
        let pad = width.saturating_sub(used + right_cols).max(GAP.len());
        let style = match uncertain {
            true => theme.tool_warning,
            false => theme.accent,
        };
        spans.push(Span::raw(SPACE.repeat(pad)));
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// `0 ├──┼──●──┤ 4` under the label column: the levels as ticks and the expected score as a mark.
fn scale(
    score: f64,
    levels: usize,
    label_cols: usize,
    cells: usize,
    theme: &Theme,
) -> Line<'static> {
    let last = levels.saturating_sub(1).max(1);
    let span = cells.saturating_sub(1).max(1);
    let position = |level: f64| ((level / last as f64) * span as f64).round() as usize;
    let mark = position(score.clamp(0.0, last as f64)).min(span);
    let track: String = (0..=span)
        .map(|cell| match cell {
            0 => SCALE_START,
            cell if cell == span => SCALE_END,
            cell if (1..last).any(|level| position(level as f64) == cell) => SCALE_TICK,
            _ => SCALE_LINE,
        })
        .collect();
    let mut chars: Vec<char> = track.chars().collect();
    let before: String = chars.drain(..mark).collect();
    let after: String = chars.into_iter().skip(1).collect();
    Line::from(vec![
        Span::raw(INDENT),
        Span::styled(padded("0", label_cols), theme.tool_dim),
        Span::raw(GAP),
        Span::styled(before, theme.tool_dim),
        Span::styled(SCALE_MARK.to_string(), theme.accent),
        Span::styled(after, theme.tool_dim),
        Span::styled(format!("{SPACE}{last}"), theme.tool_dim),
    ])
}

fn probability(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| (0.0..=1.0).contains(value))
}

/// A string as written, other content as compact JSON, and nothing for `null`.
fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        value => Some(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::theme;

    const SEVERITY: &str = "severity";
    const UNSURE_WINNER: f64 = 0.55;

    fn script_answer() -> (Value, Value) {
        let arguments = json!({
            "input": "The parser crashes on empty files.",
            "questions": [
                {"type": "predicate", "name": "parser_bug", "instructions": "A parser bug?"},
                {"type": "choice", "name": "team", "instructions": "Which team?",
                 "choices": [{"value": "core", "description": "the engine"}, "docs"]},
                {"type": "score", "name": SEVERITY, "instructions": "How bad?", "levels": ["low", "mid", "high"]},
            ],
        });
        let answer = json!({
            "model": "m",
            "answers": [
                {"type": "predicate", "name": "parser_bug", "probability": 0.91},
                {"type": "choice", "name": "team", "choice": "core", "confidence": 0.648,
                 "probabilities": [{"value": "core", "probability": 0.824}, {"value": "docs", "probability": 0.176}]},
                {"type": "score", "name": SEVERITY, "score": 1.4, "confidence": 0.5,
                 "probabilities": [{"value": 0, "label": "low", "probability": 0.1},
                                   {"value": 1, "label": "mid", "probability": UNSURE_WINNER},
                                   {"value": 2, "label": "high", "probability": 0.35}]},
            ],
        });
        (arguments, answer)
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test_case(0.0, "0.0%" ; "zero")]
    #[test_case(0.0005, "<0.1%" ; "nearly_never")]
    #[test_case(0.824, "82.4%" ; "plain")]
    #[test_case(0.9995, ">99.9%" ; "nearly_sure")]
    #[test_case(1.0, "100.0%" ; "sure")]
    fn pct_never_rounds_to_certainty(probability: f64, expected: &str) {
        assert_eq!(pct(probability), expected);
    }

    #[test_case(0.5, 4, "\u{2588}\u{2588}", "\u{2591}\u{2591}" ; "half")]
    #[test_case(0.0, 3, "", "\u{2591}\u{2591}\u{2591}" ; "empty")]
    #[test_case(1.0, 2, "\u{2588}\u{2588}", "" ; "full")]
    #[test_case(0.25, 1, "\u{258e}", "" ; "two_eighths")]
    fn bar_fills_to_the_nearest_eighth(probability: f64, cells: usize, fill: &str, track: &str) {
        assert_eq!(bar(probability, cells), (fill.to_owned(), track.to_owned()));
    }

    #[test]
    fn a_tie_goes_to_the_option_declared_first() {
        let verdict = Verdict::predicate(SEVERITY.to_owned(), None, 0.5);
        assert_eq!(verdict.winner(), Some(0));
        assert_eq!(verdict.runner_up(), Some(1));
    }

    #[test]
    fn a_script_answer_reads_every_type_with_its_descriptions() {
        let (arguments, answer) = script_answer();
        let verdicts = from_script(Some(&arguments), &answer).unwrap();
        assert_eq!(
            summary(&verdicts),
            "parser_bug yes 91.0% \u{b7} team core 82.4% \u{b7} severity mid 55.0%"
        );
        assert_eq!(
            verdicts[1].options[0].description.as_deref(),
            Some("the engine")
        );
        assert_eq!(verdicts[1].instructions.as_deref(), Some("Which team?"));
        assert_eq!(verdicts[2].score, Some(1.4));
    }

    #[test_case(json!({"answers": [{"type": "rank", "name": "x"}]}) ; "unknown_type")]
    #[test_case(json!({"answers": []}) ; "no_answers")]
    #[test_case(json!([{"index": 0, "score": 0.5}]) ; "a_rerank")]
    #[test_case(json!({"answers": [{"type": "predicate", "name": "x", "probability": 2}]}) ; "out_of_range")]
    fn an_unknown_shape_is_not_a_script_answer(answer: Value) {
        assert_eq!(from_script(None, &answer), None);
    }

    #[test]
    fn rows_bar_every_option_and_mark_the_winner() {
        let (arguments, answer) = script_answer();
        let verdicts = from_script(Some(&arguments), &answer).unwrap();
        let theme = theme::current();
        let lines = rows(&verdicts, 80, &theme);
        let drawn = text(&lines);
        let team = drawn
            .iter()
            .position(|line| line.starts_with("team"))
            .unwrap();
        assert!(drawn[team].ends_with("core 82.4%"), "{drawn:?}");
        assert!(drawn[team].contains(CHOICE_TAG), "{drawn:?}");
        let core = &lines[team + 2];
        assert_eq!(
            core.spans[1].style,
            theme.accent.add_modifier(Modifier::BOLD)
        );
        assert!(core.to_string().contains("82.4%"), "{drawn:?}");
        assert_eq!(drawn[team + 3].trim(), "the engine");
        assert!(drawn.contains(&format!("{INDENT}{CONFIDENCE_PREFIX}0.6480")));
        assert!(drawn.iter().all(|line| line.width() <= 80), "{drawn:?}");
    }

    #[test]
    fn an_uncertain_winner_warns_and_names_the_runner_up() {
        let (arguments, answer) = script_answer();
        let verdicts = from_script(Some(&arguments), &answer).unwrap();
        let theme = theme::current();
        let lines = rows(&verdicts[2..], 80, &theme);
        let headline = &lines[0];
        assert_eq!(headline.spans.last().unwrap().style, theme.tool_warning);
        let drawn = text(&lines);
        assert!(
            drawn.contains(
                &"score 1.40 on 0\u{2013}2 \u{b7} uncertain, runner-up 2 high 35.0%".to_owned()
            ),
            "{drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.contains(SCALE_MARK)),
            "{drawn:?}"
        );
    }

    #[test]
    fn a_logged_decision_reads_the_engine_shape() {
        let questions = json!({
            "fit": {"type": "choice", "instructions": "Which?", "criteria": {"b": "second", "a": null}},
            "level": {"type": "score", "instructions": "How much?", "criteria": ["none", "some"]},
            "ok": {"type": "noul", "instructions": "Fine?", "criteria": {"true": "it is", "false": "it is not"}},
        });
        let answers = json!({
            "fit": {"type": "choice", "choice": "b", "probabilities": {"a": 0.3, "b": 0.7}, "confidence": 0.4},
            "level": {"type": "score", "score": 0.2, "legend": {"0": "none", "1": "some"},
                      "probabilities": {"0": 0.8, "1": 0.2}, "confidence": 0.6},
            "ok": {"type": "noul", "noul": 0.95},
        });
        let verdicts = from_logged(&questions, &answers).unwrap();
        assert_eq!(
            summary(&verdicts),
            "fit b 70.0% \u{b7} level none 80.0% \u{b7} ok yes 95.0%"
        );
        assert_eq!(
            verdicts[0].options[0].description.as_deref(),
            Some("second")
        );
        assert_eq!(verdicts[1].options[0].description, None);
        assert_eq!(verdicts[2].options[1].description.as_deref(), Some("it is"));
    }

    #[test]
    fn a_logged_rerank_batch_is_named_by_its_candidates() {
        let questions = json!({
            "c00": {"type": "noul", "instructions": {"statement": "Relevant?", "candidate": "parser.rs"},
                    "criteria": {"true": "relevant", "false": "unrelated"}},
        });
        let answers = json!({"c00": {"type": "noul", "noul": 0.8}});
        let verdicts = from_logged(&questions, &answers).unwrap();
        assert_eq!(verdicts[0].name, "parser.rs");
        assert_eq!(verdicts[0].instructions.as_deref(), Some("Relevant?"));
    }

    #[test]
    fn a_logged_decision_without_answers_has_no_verdicts() {
        let questions = json!({"ok": {"type": "noul", "instructions": "Fine?"}});
        assert_eq!(from_logged(&questions, &json!({})), None);
    }
}
