use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, DecisionState, Decisions, NoteDraft, NoteOutcome,
    RERANK_BUDGET_CANDIDATES, Relevance, Rerank, Reranked, candidate_text, counted,
    redacted_excerpt, redacted_leaves, score_line,
};
use crate::CancelToken;
use crate::memory::search::significant_words;
use crate::permissions::PermissionManager;
use crate::tools::TOOL_OUTPUT_TOOL_NAME;
use crate::tools::native::tool_output::MAX_READ_LIMIT;
use crate::types::{TodoItem, TodoStatus};

pub(super) const MIN_WINDOW_LINES: usize = 200;
/// Most windows one pointer advice weighs: one deadline budget's worth.
pub(super) const MAX_WINDOWS: usize = RERANK_BUDGET_CANDIDATES;
const MAX_POINTERS: usize = 2;
const MAX_FOCUS_BYTES: usize = 900;
const MAX_FOCUS_TODOS: usize = 3;
const REQUEST_BYTES: usize = 400;
const TODO_BYTES: usize = 120;
const TOOL_BYTES: usize = 64;
const INPUT_BYTES: usize = 300;
const EXCERPT_BYTES: usize = 600;
const RAW_TEXT_FACTOR: usize = 2;
const SCANNED_BYTES_PER_WINDOW: usize = 64 * 1024;
const REQUEST_KEY: &str = "request";
const OPEN_TODOS_KEY: &str = "open_todos";
const TOOL_KEY: &str = "tool";
const INPUT_KEY: &str = "input";
const EXCERPT_KEY: &str = "excerpt";
const LINE_BREAK: &str = "\n";
const POINTED_NOWHERE: &str = "pointed to no omitted lines";
const RANGE_ONE: &str = "omitted line range";
const RANGE_MANY: &str = "omitted line ranges";
const POINTED: &str = "pointed";
const NOT_POINTED: &str = "not pointed";
const RELEVANCE: Relevance = Relevance {
    statement: "The excerpt comes from lines a long tool result left out of its preview, and reading those lines would help the work the query describes.",
    relevant: "It shows errors, failures, values, paths or code that the request, the open todos or the tool call are looking for.",
    unrelated: "It is progress, boilerplate or passing output, or only shares words with the query.",
};

/// A saved result whose preview left `lines` of `text` out, and the call
/// that produced it.
pub struct OmittedOutput<'a> {
    pub focus: &'a Value,
    pub tool: &'a str,
    pub input: &'a Value,
    pub text: &'a str,
    pub lines: RangeInclusive<usize>,
}

/// The windows the omitted lines were split into, and the ones worth naming:
/// at most [`MAX_POINTERS`], each scored at least the routing confidence,
/// most relevant first. Only `Acted` advice reaches the model.
pub struct PointerAdvice {
    pub windows: Vec<RangeInclusive<usize>>,
    pub advised: Vec<usize>,
    pub outcome: NoteOutcome,
    reranked: Reranked,
}

struct Window {
    lines: RangeInclusive<usize>,
    excerpt: String,
}

#[cfg(test)]
impl PointerAdvice {
    /// Every window, most relevant first.
    pub fn order(&self) -> Vec<usize> {
        self.reranked.order()
    }
}

/// What omitted lines are judged against: the latest request and the open
/// todos, in-progress first, redacted and small enough to leave the state
/// room for the tool call. `None` when there is neither.
pub(crate) fn output_focus(todos: &[TodoItem], request: Option<&str>) -> Option<Value> {
    let mut focus = json!({});
    if let Some(request) = request
        .map(|request| redacted_excerpt(request, REQUEST_BYTES))
        .filter(|request| !request.trim().is_empty())
    {
        insert_fitting(&mut focus, REQUEST_KEY, Value::String(request));
    }
    let mut open = Vec::new();
    for todo in [TodoStatus::InProgress, TodoStatus::Pending]
        .iter()
        .flat_map(|status| todos.iter().filter(move |todo| todo.status == *status))
        .take(MAX_FOCUS_TODOS)
    {
        open.push(Value::String(redacted_excerpt(&todo.content, TODO_BYTES)));
        if !insert_fitting(&mut focus, OPEN_TODOS_KEY, Value::Array(open.clone())) {
            break;
        }
    }
    focus
        .as_object()
        .is_some_and(|fields| !fields.is_empty())
        .then_some(focus)
}

/// Names the omitted windows most worth reading. Without an answer, the
/// failure a row reports if it is fresh, and nothing when the feature is off,
/// nothing could be judged or the call was cut short.
pub async fn advise_pointers(
    output: OmittedOutput<'_>,
    decisions: &Decisions,
    context: &DecisionContext,
    permissions: Option<&PermissionManager>,
    cancel: &CancelToken,
) -> Result<PointerAdvice, Option<String>> {
    let feature = DecisionFeature::OutputPointers;
    if !decisions.enabled(&feature) {
        return Err(None);
    }
    let threshold = feature
        .rerank_threshold(&decisions.config().thresholds)
        .ok_or(None)?;
    let query = query(output.focus, output.tool, output.input).ok_or(None)?;
    let mut query_words = words(output.focus);
    query_words.extend(words(output.input));
    let windows = windows(output.text, &output.lines, &query_words);
    let candidates: Vec<Value> = windows
        .iter()
        .map(|window| candidate(&window.excerpt))
        .collect();
    let (reranked, outcome) = decisions
        .rerank(
            Rerank {
                feature,
                relevance: &RELEVANCE,
                query: &query,
                candidates: &candidates,
            },
            context,
            permissions,
            cancel,
        )
        .await
        .scored()?;
    let scores = reranked.scores();
    let advised = reranked
        .order()
        .into_iter()
        .take_while(|index| scores[*index] >= threshold)
        .take(MAX_POINTERS)
        .collect();
    Ok(PointerAdvice {
        windows: windows.into_iter().map(|window| window.lines).collect(),
        advised,
        outcome,
        reranked,
    })
}

/// One pointer line per advised window, most relevant first, and `Advised`
/// recorded; `None` when no window cleared the bar or the advice is only a
/// shadow's. The row says what was or would have been pointed to, or that
/// the engine newly failed.
pub async fn pointer_lines(
    output: OmittedOutput<'_>,
    output_id: &str,
    decisions: &Decisions,
    context: &DecisionContext,
    permissions: Option<&PermissionManager>,
    cancel: &CancelToken,
) -> (Option<String>, Option<NoteDraft>) {
    let feature = DecisionFeature::OutputPointers;
    let advice = match advise_pointers(output, decisions, context, permissions, cancel).await {
        Ok(advice) => advice,
        Err(failure) => {
            let note = failure
                .map(|failure| NoteDraft::fell_back(decisions, feature, &failure, POINTED_NOWHERE));
            return (None, note);
        }
    };
    if !advice.reranked.record_advised(&advice.advised) {
        return (None, None);
    }
    let summary = format!(
        "pointed to {}",
        counted(advice.advised.len(), RANGE_ONE, RANGE_MANY)
    );
    let note = NoteDraft::new(
        decisions,
        feature,
        advice.outcome,
        &summary,
        window_lines(&advice),
    );
    let lines = (advice.outcome == NoteOutcome::Acted).then(|| {
        let lines: Vec<String> = advice
            .advised
            .iter()
            .map(|index| pointer(output_id, &advice.windows[*index]))
            .collect();
        lines.join(LINE_BREAK)
    });
    (lines, Some(note))
}

/// Every window, most relevant first: its score, its lines and whether it
/// was pointed to.
fn window_lines(advice: &PointerAdvice) -> Vec<String> {
    let scores = advice.reranked.scores();
    advice
        .reranked
        .order()
        .into_iter()
        .map(|index| {
            let lines = &advice.windows[index];
            let action = match advice.advised.contains(&index) {
                true => POINTED,
                false => NOT_POINTED,
            };
            score_line(
                scores[index],
                &format!("lines {}-{}", lines.start(), lines.end()),
                action,
            )
        })
        .collect()
}

fn pointer(output_id: &str, lines: &RangeInclusive<usize>) -> String {
    let (first, last) = (*lines.start(), *lines.end());
    let limit = (last + 1 - first).min(MAX_READ_LIMIT);
    format!(
        "[Likely relevant omitted lines {first}-{last}: {TOOL_OUTPUT_TOOL_NAME}(output_id=\"{output_id}\", offset={first}, limit={limit})]"
    )
}

/// The omitted lines in at most [`MAX_WINDOWS`] windows of at least
/// [`MIN_WINDOW_LINES`] lines, each with its excerpt.
fn windows(text: &str, omitted: &RangeInclusive<usize>, words: &BTreeSet<String>) -> Vec<Window> {
    let (first, last) = (*omitted.start(), *omitted.end());
    let size = MIN_WINDOW_LINES.max((last + 1 - first).div_ceil(MAX_WINDOWS));
    let mut lines = text.split(LINE_BREAK).skip(first - 1);
    (first..=last)
        .step_by(size)
        .map(|start| {
            let end = (start + size - 1).min(last);
            Window {
                lines: start..=end,
                excerpt: excerpt(lines.by_ref().take(end + 1 - start), words),
            }
        })
        .collect()
}

/// The window's lines sharing the most words with the query, in their own
/// order, or its first lines when none does. Only the first
/// [`SCANNED_BYTES_PER_WINDOW`] bytes are matched, so a huge window costs no
/// more than a large one.
fn excerpt<'a>(lines: impl Iterator<Item = &'a str>, words: &BTreeSet<String>) -> String {
    let mut scanned = 0;
    let mut leading = Vec::new();
    let mut leading_bytes = 0;
    let mut matched = Vec::new();
    for (position, line) in lines.enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if leading_bytes < EXCERPT_BYTES {
            leading.push(line);
            leading_bytes += line.len() + LINE_BREAK.len();
        }
        if scanned < SCANNED_BYTES_PER_WINDOW {
            let scanned_line =
                &line[..line.floor_char_boundary(SCANNED_BYTES_PER_WINDOW - scanned)];
            scanned += scanned_line.len();
            let score = significant_words(scanned_line).intersection(words).count();
            if score > 0 {
                matched.push((Reverse(score), position, line));
            }
        }
    }
    let chosen = if matched.is_empty() {
        leading
    } else {
        matched.sort_unstable();
        let mut bytes = 0;
        let mut best: Vec<(usize, &str)> = matched
            .into_iter()
            .take_while(|(_, _, line)| {
                let room = bytes < EXCERPT_BYTES;
                bytes += line.len() + LINE_BREAK.len();
                room
            })
            .map(|(_, position, line)| (position, line))
            .collect();
        best.sort_unstable();
        best.into_iter().map(|(_, line)| line).collect()
    };
    let raw: Vec<&str> = chosen.into_iter().map(raw_window).collect();
    bounded(&raw.join(LINE_BREAK), EXCERPT_BYTES)
}

/// The engine's question about one window, shortened until escaping cannot
/// push it past the bounded state.
fn candidate(excerpt: &str) -> Value {
    let mut excerpt = excerpt.to_owned();
    loop {
        let candidate = json!({EXCERPT_KEY: excerpt});
        if excerpt.is_empty() || DecisionState::new(&candidate).is_ok() {
            return candidate;
        }
        excerpt.truncate(excerpt.floor_char_boundary(excerpt.len() / 2));
    }
}

/// The focus with the tool and a redacted excerpt of its input, the input cut
/// until the whole state fits.
fn query(focus: &Value, tool: &str, input: &Value) -> Option<Value> {
    let Value::Object(fields) = focus else {
        return None;
    };
    let mut query = fields.clone();
    query.insert(TOOL_KEY.into(), Value::String(bounded(tool, TOOL_BYTES)));
    let mut excerpt = bounded(
        &redacted_leaves(input, INPUT_BYTES).to_string(),
        INPUT_BYTES,
    );
    loop {
        query.insert(INPUT_KEY.into(), Value::String(excerpt.clone()));
        let state = Value::Object(query.clone());
        if DecisionState::new(&state).is_ok() {
            return Some(state);
        }
        if excerpt.is_empty() {
            return None;
        }
        excerpt.truncate(excerpt.floor_char_boundary(excerpt.len() / 2));
    }
}

fn words(value: &Value) -> BTreeSet<String> {
    match value {
        Value::String(text) => significant_words(raw_window(text)),
        Value::Array(values) => values.iter().flat_map(words).collect(),
        Value::Object(fields) => fields.values().flat_map(words).collect(),
        _ => BTreeSet::new(),
    }
}

fn raw_window(text: &str) -> &str {
    &text[..text.floor_char_boundary(EXCERPT_BYTES * RAW_TEXT_FACTOR)]
}

fn bounded(text: &str, limit: usize) -> String {
    candidate_text(
        &text[..text.floor_char_boundary(limit * RAW_TEXT_FACTOR)],
        limit,
    )
}

fn insert_fitting(focus: &mut Value, key: &str, value: Value) -> bool {
    let mut grown = focus.clone();
    grown[key] = value;
    let fits = grown.to_string().len() <= MAX_FOCUS_BYTES;
    if fits {
        *focus = grown;
    }
    fits
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{DecisionEndpoint, DecisionProtocol};
    use std::ops::RangeInclusive;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::DecisionEffect;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        EXCERPT_KEY, LINE_BREAK, MAX_POINTERS, MAX_WINDOWS, MIN_WINDOW_LINES, OPEN_TODOS_KEY,
        OmittedOutput, POINTED_NOWHERE, REQUEST_KEY, output_focus, pointer, pointer_lines, windows,
        words,
    };
    use crate::CancelToken;
    use crate::decisions::{
        CANDIDATE_KEY, DecisionContext, Decisions, ENGINE_UNREACHABLE, NoteDraft, NoteOutcome,
    };
    use crate::memory::relevance::tests::assert_effects;
    use crate::tools::native::tests::tempdir;
    use crate::types::{TodoItem, TodoPriority, TodoStatus};

    const BASE_URL: &str = "http://127.0.0.1:1";
    const CONFIDENT: f64 = 0.99;
    const RELEVANT: f64 = 0.95;
    const BARELY_RELEVANT: f64 = 0.92;
    const RELATED: f64 = 0.5;
    const UNRELATED: f64 = 0.02;
    const OUTPUT_ID: &str = "output-1";
    const TOOL: &str = "shell";
    const COMMAND_KEY: &str = "command";
    const COMMAND: &str = "make deploy";
    const REQUEST: &str = "fix the failed deploy";
    const SECRET: &str = "do-not-send-this-value";
    const TOTAL_LINES: usize = 1_000;
    const OMITTED_FIRST: usize = 21;
    const OMITTED_LAST: usize = 980;
    const ALPHA: &str = "deploy failed: alpha";
    const BETA: &str = "deploy failed: beta";
    const GAMMA: &str = "deploy failed: gamma";
    const ALPHA_LINE: usize = 300;
    const BETA_LINE: usize = 700;
    const GAMMA_LINE: usize = 900;
    const ADVISED_POINTERS: &str = "[Likely relevant omitted lines 621-820: tool_output(output_id=\"output-1\", offset=621, limit=200)]\n[Likely relevant omitted lines 221-420: tool_output(output_id=\"output-1\", offset=221, limit=200)]";
    const POINTED_TWO: &str = "pointed to 2 omitted line ranges";
    const SHADOW_POINTED_ONE: &str = "would have pointed to 1 omitted line range";
    const ADVISED_WINDOWS: [&str; 5] = [
        "0.99  lines 621-820  pointed",
        "0.95  lines 221-420  pointed",
        "0.92  lines 821-980  not pointed",
        "0.02  lines 21-220  not pointed",
        "0.02  lines 421-620  not pointed",
    ];
    const HUGE_OMISSION: usize = 100_000;
    const FIRST_TODO: &str = "reproduce the failure";
    const DONE_TODO: &str = "read the deploy script";
    const ACTIVE_TODO: &str = "fix the deploy key";
    const SECOND_TODO: &str = "add a regression test";
    const THIRD_TODO: &str = "update the changelog";

    struct WindowEngine {
        scores: Vec<(&'static str, f64)>,
        fail: bool,
        requests: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DecisionEngine for WindowEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::to_string(request).unwrap());
            if self.fail {
                return Err(DecisionError::Unreachable);
            }
            let answers = request
                .questions
                .iter()
                .map(|(id, question)| {
                    let excerpt = question.instructions[CANDIDATE_KEY][EXCERPT_KEY]
                        .as_str()
                        .unwrap_or_default();
                    let noul = self
                        .scores
                        .iter()
                        .find(|(marker, _)| excerpt.contains(marker))
                        .map_or(UNRELATED, |(_, score)| *score);
                    (id.clone(), Answer::Noul(NoulAnswer { noul }))
                })
                .collect();
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                cache_hit: false,
            })
        }
    }

    struct Fixture {
        decisions: Decisions,
        requests: Arc<Mutex<Vec<String>>>,
        root: TempDir,
    }

    impl Fixture {
        fn new(mode: FeatureMode, scores: &[(&'static str, f64)], fail: bool) -> Self {
            let root = tempdir();
            let requests = Arc::default();
            let mut config = DecisionsConfig {
                endpoint: Some(DecisionEndpoint::new(
                    DecisionProtocol::TypeSafe.as_str(),
                    DecisionProtocol::TypeSafe,
                    BASE_URL.parse().unwrap(),
                )),
                log: true,
                ..Default::default()
            };
            config.features.output_pointers = mode;
            let engine = WindowEngine {
                scores: scores.to_vec(),
                fail,
                requests: Arc::clone(&requests),
            };
            let decisions =
                Decisions::with_engine(config, &StateDir::from_path(root.path().into()), engine)
                    .unwrap();
            Self {
                decisions,
                requests,
                root,
            }
        }

        async fn pointers(
            &self,
            text: &str,
            input: &Value,
            cancel: &CancelToken,
        ) -> (Option<String>, Option<NoteDraft>) {
            let focus = output_focus(&[], Some(REQUEST)).unwrap();
            pointer_lines(
                OmittedOutput {
                    focus: &focus,
                    tool: TOOL,
                    input,
                    text,
                    lines: OMITTED_FIRST..=OMITTED_LAST,
                },
                OUTPUT_ID,
                &self.decisions,
                &DecisionContext::default(),
                None,
                cancel,
            )
            .await
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn output_text(markers: &[(usize, &str)]) -> String {
        (1..=TOTAL_LINES)
            .map(|line| {
                markers
                    .iter()
                    .find(|(marker_line, _)| *marker_line == line)
                    .map_or_else(|| format!("step {line} ok"), |(_, text)| (*text).to_owned())
            })
            .collect::<Vec<_>>()
            .join(LINE_BREAK)
    }

    fn command() -> Value {
        json!({COMMAND_KEY: COMMAND})
    }

    fn todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_owned(),
            status,
            priority: TodoPriority::default(),
        }
    }

    #[test]
    fn pointers_name_at_most_two_confident_windows_best_first() {
        smol::block_on(async {
            let fixture = Fixture::new(
                FeatureMode::Advise,
                &[
                    (BETA, CONFIDENT),
                    (ALPHA, RELEVANT),
                    (GAMMA, BARELY_RELEVANT),
                ],
                false,
            );
            let text = output_text(&[(ALPHA_LINE, ALPHA), (BETA_LINE, BETA), (GAMMA_LINE, GAMMA)]);

            let (pointers, row) = fixture
                .pointers(&text, &command(), &CancelToken::none())
                .await;

            let pointers = pointers.unwrap();
            assert_eq!(pointers, ADVISED_POINTERS);
            assert_eq!(pointers.lines().count(), MAX_POINTERS);
            let row = row.unwrap();
            assert_eq!(row.outcome, NoteOutcome::Acted);
            assert_eq!(row.summary, POINTED_TWO);
            assert_eq!(row.details, ADVISED_WINDOWS);
            assert_effects(fixture.root.path(), &[DecisionEffect::Advised]).await;
        });
    }

    #[test_case(FeatureMode::Advise, RELATED, false, 1, None; "nothing_confident")]
    #[test_case(FeatureMode::Advise, CONFIDENT, true, 1, Some(NoteOutcome::FellBack); "engine_failed")]
    #[test_case(FeatureMode::Shadow, CONFIDENT, false, 1, Some(NoteOutcome::Shadow); "shadow_only_evaluates")]
    #[test_case(FeatureMode::Off, CONFIDENT, false, 0, None; "off_never_asks")]
    fn pointers_without_a_confident_answer_name_nothing(
        mode: FeatureMode,
        score: f64,
        fail: bool,
        requests: usize,
        outcome: Option<NoteOutcome>,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new(mode, &[(BETA, score)], fail);
            let text = output_text(&[(BETA_LINE, BETA)]);

            let (pointers, row) = fixture
                .pointers(&text, &command(), &CancelToken::none())
                .await;

            assert_eq!(pointers, None);
            assert_eq!(row.map(|row| row.outcome), outcome);
            assert_eq!(fixture.requests().len(), requests);
        });
    }

    #[test]
    fn a_shadow_row_says_what_would_have_been_pointed_to() {
        smol::block_on(async {
            let fixture = Fixture::new(FeatureMode::Shadow, &[(BETA, CONFIDENT)], false);

            let (pointers, row) = fixture
                .pointers(
                    &output_text(&[(BETA_LINE, BETA)]),
                    &command(),
                    &CancelToken::none(),
                )
                .await;

            assert_eq!(pointers, None);
            assert_eq!(row.unwrap().summary, SHADOW_POINTED_ONE);
            assert_effects(fixture.root.path(), &[]).await;
        });
    }

    #[test]
    fn a_failing_engine_falls_back_once_until_it_answers() {
        smol::block_on(async {
            let fixture = Fixture::new(FeatureMode::Advise, &[(BETA, CONFIDENT)], true);
            let text = output_text(&[(BETA_LINE, BETA)]);
            let mut summaries = Vec::new();
            for _ in 0..2 {
                let (_, row) = fixture
                    .pointers(&text, &command(), &CancelToken::none())
                    .await;
                summaries.push(row.map(|row| row.summary));
            }

            assert_eq!(
                summaries,
                [
                    Some(format!("{ENGINE_UNREACHABLE}; {POINTED_NOWHERE}")),
                    None
                ]
            );
        });
    }

    #[test]
    fn a_cancelled_call_names_nothing() {
        smol::block_on(async {
            let fixture = Fixture::new(FeatureMode::Advise, &[(BETA, CONFIDENT)], false);
            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let pointed = fixture
                .pointers(&output_text(&[(BETA_LINE, BETA)]), &command(), &cancel)
                .await;

            assert_eq!(pointed, (None, None));
        });
    }

    #[test]
    fn a_row_never_carries_the_output_or_a_secret() {
        smol::block_on(async {
            let fixture = Fixture::new(FeatureMode::Advise, &[(BETA, CONFIDENT)], false);
            let leaked = format!("{BETA} TOKEN={SECRET}");
            let input = json!({COMMAND_KEY: format!("{COMMAND} TOKEN={SECRET}")});

            let (_, row) = fixture
                .pointers(
                    &output_text(&[(BETA_LINE, &leaked)]),
                    &input,
                    &CancelToken::none(),
                )
                .await;

            let row = row.unwrap();
            let text = format!("{} {}", row.summary, row.details.join(LINE_BREAK));
            assert!(!text.contains(SECRET));
            assert!(!text.contains(BETA));
        });
    }

    #[test]
    fn omitted_lines_and_the_tool_input_are_redacted_before_they_are_sent() {
        smol::block_on(async {
            let fixture = Fixture::new(FeatureMode::Advise, &[(BETA, CONFIDENT)], false);
            let leaked = format!("{BETA} TOKEN={SECRET}");
            let input = json!({COMMAND_KEY: format!("{COMMAND} TOKEN={SECRET}")});

            let (pointers, _) = fixture
                .pointers(
                    &output_text(&[(BETA_LINE, &leaked)]),
                    &input,
                    &CancelToken::none(),
                )
                .await;

            assert!(pointers.is_some());
            let requests = fixture.requests();
            assert!(!requests.is_empty());
            assert!(requests.iter().all(|request| !request.contains(SECRET)));
        });
    }

    #[test_case(1, 50, 1; "short_omission_is_one_window")]
    #[test_case(OMITTED_FIRST, OMITTED_LAST, 5; "windows_keep_the_minimum_size")]
    #[test_case(1, HUGE_OMISSION, MAX_WINDOWS; "huge_omission_fills_one_batch")]
    fn windows_tile_the_omitted_lines(first: usize, last: usize, count: usize) {
        let text = vec!["step ok"; last + 1].join(LINE_BREAK);

        let windows = windows(&text, &(first..=last), &words(&json!(REQUEST)));

        assert_eq!(windows.len(), count);
        assert_eq!(*windows[0].lines.start(), first);
        assert_eq!(*windows[count - 1].lines.end(), last);
        for pair in windows.windows(2) {
            assert_eq!(*pair[1].lines.start(), pair[0].lines.end() + 1);
            assert!(pair[0].lines.clone().count() >= MIN_WINDOW_LINES);
        }
    }

    #[test]
    fn an_excerpt_keeps_the_lines_sharing_query_words() {
        let text = output_text(&[(ALPHA_LINE, ALPHA)]);

        let windows = windows(
            &text,
            &(OMITTED_FIRST..=OMITTED_LAST),
            &words(&json!(REQUEST)),
        );

        assert_eq!(windows[1].excerpt, ALPHA);
        assert!(
            windows[0]
                .excerpt
                .starts_with(&format!("step {OMITTED_FIRST} ok"))
        );
    }

    #[test]
    fn a_focus_holds_the_request_and_open_todos_in_progress_first() {
        let todos = [
            todo(FIRST_TODO, TodoStatus::Pending),
            todo(DONE_TODO, TodoStatus::Completed),
            todo(ACTIVE_TODO, TodoStatus::InProgress),
            todo(SECOND_TODO, TodoStatus::Pending),
            todo(THIRD_TODO, TodoStatus::Pending),
        ];

        let focus = output_focus(&todos, Some(REQUEST)).unwrap();

        assert_eq!(focus[REQUEST_KEY], REQUEST);
        assert_eq!(
            focus[OPEN_TODOS_KEY],
            json!([ACTIVE_TODO, FIRST_TODO, SECOND_TODO])
        );
    }

    #[test]
    fn a_focus_without_a_request_or_open_todos_is_none() {
        assert_eq!(
            output_focus(&[todo(DONE_TODO, TodoStatus::Completed)], None),
            None
        );
    }

    #[test_case(221..=420, "[Likely relevant omitted lines 221-420: tool_output(output_id=\"output-1\", offset=221, limit=200)]"; "whole_window")]
    #[test_case(1..=5_000, "[Likely relevant omitted lines 1-5000: tool_output(output_id=\"output-1\", offset=1, limit=2000)]"; "limit_capped_at_one_read")]
    fn a_pointer_names_a_tool_output_read(lines: RangeInclusive<usize>, expected: &str) {
        assert_eq!(pointer(OUTPUT_ID, &lines), expected);
    }
}
