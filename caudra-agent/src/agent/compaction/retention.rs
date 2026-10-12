use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use caudra_config::decisions::FeatureMode;
use caudra_providers::{ContentBlock, Message};
use serde_json::{Value, json};

use super::{
    KEEP_LAST_TOOL_RESULTS, RETAINED_TOOL_RESULT_MAX_BYTES, utf8_prefix_end, utf8_suffix_start,
};
use crate::CancelToken;
use crate::agent::{GoalHandle, History};
use crate::decisions::{
    DecisionContext, DecisionFeature, DecisionState, Decisions, MAX_RERANK_BATCH, Relevance,
    Rerank, Reranked, candidate_text, push_fitting, redacted_excerpt, redacted_leaves,
};
use crate::memory::search::significant_words;
use crate::permissions::PermissionManager;
use crate::types::TodoItem;

const MAX_OPEN_TODOS: usize = 3;
const GOAL_BYTES: usize = 400;
const REQUEST_BYTES: usize = 500;
const TODO_BYTES: usize = 160;
const TOOL_BYTES: usize = 64;
const INPUT_BYTES: usize = 300;
const CONTENT_BYTES: usize = 600;
const WINDOW_BYTES: usize = 600;
const MIN_LINE_WINDOW_BYTES: usize = WINDOW_BYTES / 2;
const RAW_TEXT_FACTOR: usize = 2;
const GOAL_KEY: &str = "goal";
const REQUEST_KEY: &str = "request";
const OPEN_TODOS_KEY: &str = "open_todos";
const TOOL_KEY: &str = "tool";
const INPUT_KEY: &str = "input";
const CONTENT_KEY: &str = "content";
const EXCERPT_KEY: &str = "excerpt";
const SURFACE_KEY: &str = "surface";
const SURFACE: &str = "compaction";
const SEPARATOR: &str = "\n\n";
const EXCERPT_SEPARATORS: usize = 4;
const EXCERPT_MARKER: &str = "[tool result truncated for compaction; text omitted around the excerpt most relevant to the open work";
const OUTPUT_ID_LABEL: &str = "full output ID";
const RETAIN_ID: &str = "Retain this ID in the resulting summary.";
const GAP_MARKER: &str = "[more text omitted]";
const RESULTS: Relevance = Relevance {
    statement: "The earlier tool result holds information the agent still needs for the work the query describes.",
    relevant: "It shows code, errors, paths, values or decisions that the goal, the open todos or the request depend on.",
    unrelated: "It is about finished or unrelated work, or only shares words with the query.",
};
const WINDOWS: Relevance = Relevance {
    statement: "The excerpt of a long tool result holds information the agent still needs for the work the query describes.",
    relevant: "It shows code, errors, paths or values that the goal, the open todos or the request depend on.",
    unrelated: "It is boilerplate or progress output, or about unrelated work.",
};

pub struct Retention {
    pub(crate) decisions: Decisions,
    pub(crate) context: DecisionContext,
    pub(crate) query: Value,
    pub(crate) permissions: Option<Arc<PermissionManager>>,
}

pub(crate) struct ToolResultView<'a> {
    pub(crate) tool: &'a str,
    pub(crate) input: Option<&'a Value>,
    pub(crate) content: &'a str,
    pub(crate) output_id: Option<&'a str>,
}

pub(super) struct RetentionPlan {
    kept: Vec<usize>,
    excerpts: BTreeMap<usize, String>,
}

struct Layout<'a> {
    index: usize,
    tool: &'a str,
    content: &'a str,
    marker: String,
    head_end: usize,
    tail_start: usize,
}

impl Retention {
    pub fn new(
        permissions: &Arc<PermissionManager>,
        goal: &GoalHandle,
        history: &History,
    ) -> Option<Self> {
        let decisions = permissions
            .decisions()
            .filter(|decisions| decisions.enabled(&DecisionFeature::CompactionRetention))?;
        let query = retention_query(
            goal.active_condition().as_deref(),
            history.todos().unwrap_or_default(),
            history.latest_user_request(),
        )?;
        Some(Self {
            decisions,
            context: permissions.decision_context(json!({SURFACE_KEY: SURFACE})),
            query,
            permissions: Some(Arc::clone(permissions)),
        })
    }

    fn enforced(&self) -> bool {
        *self.decisions.mode(&DecisionFeature::CompactionRetention) == FeatureMode::Enforce
    }

    async fn rerank(
        &self,
        relevance: &Relevance,
        candidates: &[Value],
        cancel: &CancelToken,
    ) -> Option<(Reranked, f64)> {
        let feature = DecisionFeature::CompactionRetention;
        let floor = feature.rerank_threshold(&self.decisions.config().thresholds)?;
        let reranked = self
            .decisions
            .rerank(
                Rerank {
                    feature,
                    relevance,
                    query: &self.query,
                    candidates,
                },
                &self.context,
                self.permissions.as_deref(),
                cancel,
            )
            .await?;
        Some((reranked, floor))
    }
}

impl RetentionPlan {
    pub(super) fn keeps(&self, index: usize) -> bool {
        self.kept.contains(&index)
    }

    pub(super) fn excerpt(&self, index: usize) -> Option<&str> {
        self.excerpts.get(&index).map(String::as_str)
    }
}

impl<'a> Layout<'a> {
    fn new(index: usize, result: &ToolResultView<'a>) -> Option<Self> {
        let content = result.content;
        if content.len() <= RETAINED_TOOL_RESULT_MAX_BYTES {
            return None;
        }
        let marker = match result.output_id {
            Some(id) => format!("{EXCERPT_MARKER}; {OUTPUT_ID_LABEL}: {id}. {RETAIN_ID}]"),
            None => format!("{EXCERPT_MARKER}]"),
        };
        let edges = RETAINED_TOOL_RESULT_MAX_BYTES.checked_sub(
            SEPARATOR.len() * EXCERPT_SEPARATORS + marker.len() + GAP_MARKER.len() + WINDOW_BYTES,
        )?;
        Some(Self {
            index,
            tool: result.tool,
            content,
            head_end: utf8_prefix_end(content, edges.div_ceil(2)),
            tail_start: utf8_suffix_start(content, edges / 2),
            marker,
        })
    }

    fn windows(&self, words: &BTreeSet<String>) -> Vec<Range<usize>> {
        let mut windows = split_windows(self.content, self.head_end..self.tail_start);
        if windows.len() > MAX_RERANK_BATCH {
            windows.sort_by_cached_key(|window| {
                let matched = significant_words(&self.content[window.clone()])
                    .intersection(words)
                    .count();
                (Reverse(matched), window.start)
            });
            windows.truncate(MAX_RERANK_BATCH);
            windows.sort_unstable_by_key(|window| window.start);
        }
        windows
    }

    fn candidate(&self, window: &Range<usize>) -> Value {
        fitted(
            json!({
                TOOL_KEY: bounded(self.tool, TOOL_BYTES),
                EXCERPT_KEY: candidate_text(&self.content[window.clone()], WINDOW_BYTES),
            }),
            &[EXCERPT_KEY],
        )
    }

    fn excerpt(&self, window: &Range<usize>) -> String {
        let mut excerpt = String::with_capacity(RETAINED_TOOL_RESULT_MAX_BYTES);
        excerpt.push_str(&self.content[..self.head_end]);
        let mut markers = [self.marker.as_str(), GAP_MARKER].into_iter();
        for (gap, kept) in [
            (self.head_end..window.start, window.clone()),
            (
                window.end..self.tail_start,
                self.tail_start..self.content.len(),
            ),
        ] {
            if !gap.is_empty()
                && let Some(marker) = markers.next()
            {
                excerpt.push_str(SEPARATOR);
                excerpt.push_str(marker);
                excerpt.push_str(SEPARATOR);
            }
            excerpt.push_str(&self.content[kept]);
        }
        excerpt
    }
}

pub(crate) fn retention_query(
    goal: Option<&str>,
    todos: &[TodoItem],
    request: Option<&str>,
) -> Option<Value> {
    let mut query = json!({});
    insert_fitting(
        &mut query,
        GOAL_KEY,
        goal.map(|goal| redacted_excerpt(goal, GOAL_BYTES)),
    );
    insert_fitting(
        &mut query,
        REQUEST_KEY,
        request.map(|request| redacted_excerpt(request, REQUEST_BYTES)),
    );
    push_fitting(
        &mut query,
        OPEN_TODOS_KEY,
        todos
            .iter()
            .filter(|todo| todo.status.is_open())
            .take(MAX_OPEN_TODOS)
            .map(|todo| json!(redacted_excerpt(&todo.content, TODO_BYTES))),
    );
    query
        .as_object()
        .is_some_and(|fields| !fields.is_empty())
        .then_some(query)
}

pub(crate) async fn select_tool_results(
    retention: &Retention,
    results: &[ToolResultView<'_>],
    cancel: &CancelToken,
) -> Option<Vec<usize>> {
    if !swappable(results) {
        return None;
    }
    let candidates = &results[..results.len().min(MAX_RERANK_BATCH)];
    let defaults = candidates.len().min(KEEP_LAST_TOOL_RESULTS);
    let mut kept: Vec<usize> = (0..defaults)
        .filter(|index| candidates[*index].output_id.is_none())
        .collect();
    let values: Vec<Value> = candidates.iter().map(result_candidate).collect();
    let (reranked, floor) = retention.rerank(&RESULTS, &values, cancel).await?;
    let (relevant, _) = reranked.split_at_floor(floor);
    for index in relevant.into_iter().chain(0..candidates.len()) {
        if kept.len() == KEEP_LAST_TOOL_RESULTS {
            break;
        }
        if !kept.contains(&index) {
            kept.push(index);
        }
    }
    let mut shown = kept.clone();
    shown.sort_unstable();
    reranked.record_rerouted(&shown, defaults);
    Some(kept)
}

pub(super) async fn rank_tool_results(
    head: &[Message],
    retention: &Retention,
    cancel: &CancelToken,
) -> Option<RetentionPlan> {
    let results = newest_tool_results(head);
    let selected = select_tool_results(retention, &results, cancel).await;
    if selected.is_none() && swappable(&results) && retention.enforced() {
        return None;
    }
    let reranked = selected.is_some();
    let kept = selected.unwrap_or_else(|| (0..results.len().min(KEEP_LAST_TOOL_RESULTS)).collect());
    let excerpts = excerpts(retention, &results, &kept, cancel).await;
    (reranked || !excerpts.is_empty()).then_some(RetentionPlan { kept, excerpts })
}

fn swappable(results: &[ToolResultView<'_>]) -> bool {
    results.len() > KEEP_LAST_TOOL_RESULTS
        && results[..KEEP_LAST_TOOL_RESULTS]
            .iter()
            .any(|result| result.output_id.is_some())
}

fn newest_tool_results(head: &[Message]) -> Vec<ToolResultView<'_>> {
    let calls: HashMap<&str, (&str, &Value)> = head
        .iter()
        .flat_map(Message::tool_uses)
        .map(|(id, name, input)| (id, (name, input)))
        .collect();
    head.iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                output_ref,
                ..
            } => {
                let call = calls.get(tool_use_id.as_str());
                Some(ToolResultView {
                    tool: call.map_or("", |(name, _)| *name),
                    input: call.map(|(_, input)| *input),
                    content,
                    output_id: output_ref.as_ref().map(|output_ref| output_ref.id.as_str()),
                })
            }
            _ => None,
        })
        .take(MAX_RERANK_BATCH)
        .collect()
}

async fn excerpts(
    retention: &Retention,
    results: &[ToolResultView<'_>],
    kept: &[usize],
    cancel: &CancelToken,
) -> BTreeMap<usize, String> {
    let layouts: Vec<Layout<'_>> = kept
        .iter()
        .filter_map(|index| Layout::new(*index, results.get(*index)?))
        .collect();
    if layouts.is_empty() {
        return BTreeMap::new();
    }
    let words = query_words(&retention.query);
    let windows: Vec<(usize, Range<usize>)> = layouts
        .iter()
        .enumerate()
        .flat_map(|(layout, shape)| {
            shape
                .windows(&words)
                .into_iter()
                .map(move |window| (layout, window))
        })
        .collect();
    let candidates: Vec<Value> = windows
        .iter()
        .map(|(layout, window)| layouts[*layout].candidate(window))
        .collect();
    let Some((reranked, floor)) = retention.rerank(&WINDOWS, &candidates, cancel).await else {
        return BTreeMap::new();
    };
    let mut best = BTreeMap::new();
    for candidate in reranked.split_at_floor(floor).0 {
        best.entry(windows[candidate].0).or_insert(candidate);
    }
    let applied: Vec<usize> = best.values().copied().collect();
    reranked.record_rerouted(&applied, 0);
    best.into_iter()
        .map(|(layout, candidate)| {
            let layout = &layouts[layout];
            (layout.index, layout.excerpt(&windows[candidate].1))
        })
        .collect()
}

fn split_windows(content: &str, middle: Range<usize>) -> Vec<Range<usize>> {
    let mut windows = Vec::new();
    let mut start = middle.start;
    while start < middle.end {
        let limit = content.floor_char_boundary((start + WINDOW_BYTES).min(middle.end));
        let end = content[start..limit]
            .rfind('\n')
            .filter(|newline| limit < middle.end && *newline >= MIN_LINE_WINDOW_BYTES)
            .map_or(limit, |newline| start + newline + 1);
        windows.push(start..end);
        start = end;
    }
    windows
}

fn result_candidate(result: &ToolResultView<'_>) -> Value {
    let input = result
        .input
        .map(|input| redacted_leaves(input, INPUT_BYTES).to_string())
        .unwrap_or_default();
    fitted(
        json!({
            TOOL_KEY: bounded(result.tool, TOOL_BYTES),
            INPUT_KEY: bounded(&input, INPUT_BYTES),
            CONTENT_KEY: bounded(result.content, CONTENT_BYTES),
        }),
        &[CONTENT_KEY, INPUT_KEY],
    )
}

fn fitted(mut candidate: Value, keys: &[&str]) -> Value {
    for key in keys {
        while DecisionState::new(&candidate).is_err() {
            let Some(text) = candidate[*key].as_str().filter(|text| !text.is_empty()) else {
                break;
            };
            let shorter = text[..text.floor_char_boundary(text.len() / 2)].to_owned();
            candidate[*key] = Value::String(shorter);
        }
    }
    candidate
}

fn bounded(text: &str, limit: usize) -> String {
    candidate_text(
        &text[..text.floor_char_boundary(limit * RAW_TEXT_FACTOR)],
        limit,
    )
}

fn insert_fitting(query: &mut Value, key: &str, text: Option<String>) {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return;
    };
    let mut candidate = query.clone();
    candidate[key] = Value::String(text);
    if DecisionState::new(&candidate).is_ok() {
        *query = candidate;
    }
}

fn query_words(query: &Value) -> BTreeSet<String> {
    match query {
        Value::String(text) => significant_words(text),
        Value::Array(values) => values.iter().flat_map(query_words).collect(),
        Value::Object(fields) => fields.values().flat_map(query_words).collect(),
        _ => BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_providers::{ContentBlock, Message, Role};
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::DecisionEffect;
    use caudra_storage::tool_outputs::ToolOutputRef;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::super::{TOOL_RESULT_PLACEHOLDER, strip_old_tool_results};
    use super::{
        EXCERPT_MARKER, GAP_MARKER, GOAL_KEY, OPEN_TODOS_KEY, REQUEST_KEY, RETAIN_ID,
        RETAINED_TOOL_RESULT_MAX_BYTES, Retention, RetentionPlan, ToolResultView,
        rank_tool_results, result_candidate, retention_query,
    };
    use crate::CancelToken;
    use crate::decisions::{DecisionContext, DecisionState, Decisions};
    use crate::memory::relevance::tests::assert_effects;
    use crate::types::{TodoItem, TodoPriority, TodoStatus};

    const BASE_URL: &str = "http://127.0.0.1:1";
    const RELEVANT: f64 = 0.9;
    const UNRELATED: f64 = 0.02;
    const TOOL: &str = "shell";
    const COMMAND_KEY: &str = "command";
    const REQUEST: &str = "fix the failing parser test";
    const GOAL: &str = "the parser accepts empty input";
    const SECRET: &str = "do-not-send-this-value";
    const SECRET_FIELD: &str = "api_token";
    const RESULT_COUNT: usize = 6;
    const HEAD: &str = "HEAD";
    const TAIL: &str = "TAIL";
    const NEEDLE: &str = "needle parser rejects empty input";
    const FILLER: &str = "é";
    const FILLER_LINE_CHARS: usize = 30;
    const SHORT_MIDDLE_LINES: usize = 50;
    const LONG_MIDDLE_LINES: usize = 300;
    const ESCAPED_REPEATS: usize = 1_000;
    const BLOATED_UNIT: &str = "\u{1} ";
    const FIRST_TODO: &str = "reproduce the failure";
    const DONE_TODO: &str = "read the parser";
    const SECOND_TODO: &str = "fix the empty input case";
    const THIRD_TODO: &str = "add a regression test";
    const FOURTH_TODO: &str = "update the changelog";

    struct MarkerEngine {
        relevant: Vec<String>,
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DecisionEngine for MarkerEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(DecisionError::Unreachable);
            }
            let answers = request
                .questions
                .iter()
                .map(|(id, question)| {
                    let text = question.instructions.to_string();
                    let noul = match self.relevant.iter().any(|marker| text.contains(marker)) {
                        true => RELEVANT,
                        false => UNRELATED,
                    };
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

    fn engine(relevant: &[&str], fail: bool) -> (MarkerEngine, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = MarkerEngine {
            relevant: relevant.iter().map(|marker| (*marker).to_owned()).collect(),
            fail,
            calls: Arc::clone(&calls),
        };
        (engine, calls)
    }

    fn retention(root: &Path, mode: FeatureMode, engine: MarkerEngine) -> Retention {
        let mut config = DecisionsConfig {
            base_url: Some(BASE_URL.parse().unwrap()),
            log: true,
            ..Default::default()
        };
        config.features.compaction_retention = mode;
        Retention {
            decisions: Decisions::with_engine(config, &StateDir::from_path(root.into()), engine)
                .unwrap(),
            context: DecisionContext::default(),
            query: retention_query(None, &[], Some(REQUEST)).unwrap(),
            permissions: None,
        }
    }

    fn result(index: usize) -> String {
        format!("result {index:02}")
    }

    fn output_id(index: usize) -> String {
        format!("output-{index:02}")
    }

    fn results(retrievable: &[bool]) -> Vec<(String, bool)> {
        retrievable
            .iter()
            .enumerate()
            .map(|(index, retrievable)| (result(index), *retrievable))
            .collect()
    }

    fn long_result(middle_lines: usize) -> String {
        let filler = format!("{}\n", FILLER.repeat(FILLER_LINE_CHARS)).repeat(middle_lines);
        format!("{HEAD}\n{filler}{NEEDLE}\n{filler}{TAIL}")
    }

    fn head(newest_first: &[(String, bool)]) -> Vec<Message> {
        newest_first
            .iter()
            .enumerate()
            .rev()
            .flat_map(|(index, (content, retrievable))| {
                let call = format!("call-{index:02}");
                let output_ref = retrievable.then(|| ToolOutputRef {
                    id: output_id(index).parse().unwrap(),
                    byte_count: content.len(),
                    line_count: content.lines().count(),
                });
                [
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::tool_use(
                            call.as_str(),
                            TOOL,
                            json!({COMMAND_KEY: format!("cat file-{index:02}")}),
                        )],
                        ..Default::default()
                    },
                    Message {
                        role: Role::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: call,
                            content: content.clone(),
                            is_error: false,
                            output_ref,
                        }],
                        ..Default::default()
                    },
                ]
            })
            .collect()
    }

    fn rank(
        head: &[Message],
        retention: &Retention,
        cancel: &CancelToken,
    ) -> Option<RetentionPlan> {
        smol::block_on(rank_tool_results(head, retention, cancel))
    }

    fn stripped(head: &[Message], plan: Option<&RetentionPlan>) -> Vec<String> {
        let mut messages = head.to_vec();
        strip_old_tool_results(&mut messages, plan);
        messages
            .iter()
            .rev()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
            priority: TodoPriority::default(),
        }
    }

    #[test_case(&[true; RESULT_COUNT], &[4], &[0, 1, 4]; "a_relevant_older_result_replaces_a_newer_irrelevant_one")]
    #[test_case(&[true; RESULT_COUNT], &[1, 3, 4, 5], &[1, 3, 4]; "ties_go_to_the_newer_result")]
    #[test_case(&[false, true, true, true, true, true], &[3, 4, 5], &[0, 3, 4]; "a_result_without_an_output_id_is_never_newly_dropped")]
    #[test_case(&[true, true, true, false, true, true], &[3], &[0, 1, 3]; "an_older_result_without_an_output_id_can_be_kept")]
    fn relevant_results_are_kept_in_place_of_newer_ones(
        retrievable: &[bool],
        relevant: &[usize],
        kept: &[usize],
    ) {
        let state = tempfile::tempdir().unwrap();
        let markers: Vec<String> = relevant.iter().copied().map(result).collect();
        let markers: Vec<&str> = markers.iter().map(String::as_str).collect();
        let (engine, calls) = engine(&markers, false);
        let retention = retention(state.path(), FeatureMode::Enforce, engine);
        let head = head(&results(retrievable));

        let plan = rank(&head, &retention, &CancelToken::none());

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        for (index, content) in stripped(&head, plan.as_ref()).iter().enumerate() {
            match (kept.contains(&index), retrievable[index]) {
                (true, _) => assert_eq!(*content, result(index)),
                (false, true) => assert!(content.contains(&output_id(index))),
                (false, false) => assert_eq!(content, TOOL_RESULT_PLACEHOLDER),
            }
        }
    }

    #[test_case(&[false; RESULT_COUNT]; "the_newest_results_have_no_output_id")]
    #[test_case(&[true; 3]; "nothing_older_to_keep")]
    fn nothing_to_swap_asks_nothing(retrievable: &[bool]) {
        let state = tempfile::tempdir().unwrap();
        let (engine, calls) = engine(&[&result(RESULT_COUNT - 1)], false);
        let retention = retention(state.path(), FeatureMode::Enforce, engine);

        let plan = rank(
            &head(&results(retrievable)),
            &retention,
            &CancelToken::none(),
        );

        assert!(plan.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test_case(4, DecisionEffect::Rerouted; "changed")]
    #[test_case(0, DecisionEffect::None; "unchanged")]
    fn rerouted_is_recorded_only_when_the_kept_results_change(
        relevant: usize,
        effect: DecisionEffect,
    ) {
        smol::block_on(async {
            let state = tempfile::tempdir().unwrap();
            let (engine, _) = engine(&[&result(relevant)], false);
            let retention = retention(state.path(), FeatureMode::Enforce, engine);

            rank_tool_results(
                &head(&results(&[true; RESULT_COUNT])),
                &retention,
                &CancelToken::none(),
            )
            .await
            .unwrap();

            assert_effects(state.path(), &[effect]).await;
        });
    }

    #[test_case(FeatureMode::Off, false, false, 0; "off")]
    #[test_case(FeatureMode::Shadow, false, false, 2; "shadow")]
    #[test_case(FeatureMode::Enforce, true, false, 1; "engine_failure_skips_the_window_request")]
    #[test_case(FeatureMode::Enforce, false, true, 0; "cancelled")]
    fn without_an_answer_results_are_stripped_as_before(
        mode: FeatureMode,
        fail: bool,
        cancelled: bool,
        asked: usize,
    ) {
        let state = tempfile::tempdir().unwrap();
        let (engine, calls) = engine(&[NEEDLE, &result(4)], fail);
        let retention = retention(state.path(), mode, engine);
        let mut newest_first = results(&[true; RESULT_COUNT]);
        newest_first[0].0 = long_result(SHORT_MIDDLE_LINES);
        let (trigger, cancel) = CancelToken::new();
        if cancelled {
            trigger.cancel();
        }

        let plan = rank(&head(&newest_first), &retention, &cancel);

        assert!(plan.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), asked);
    }

    #[test_case(json!({COMMAND_KEY: format!("deploy TOKEN={SECRET}")}); "secret_inside_a_quoted_command")]
    #[test_case(json!({SECRET_FIELD: SECRET}); "secret_named_field")]
    fn tool_inputs_are_redacted_before_they_are_sent(input: Value) {
        let candidate = result_candidate(&ToolResultView {
            tool: TOOL,
            input: Some(&input),
            content: REQUEST,
            output_id: None,
        });

        assert!(!candidate.to_string().contains(SECRET));
    }

    #[test_case(SHORT_MIDDLE_LINES, true; "with_an_output_id")]
    #[test_case(SHORT_MIDDLE_LINES, false; "without_an_output_id")]
    #[test_case(LONG_MIDDLE_LINES, true; "a_long_middle_is_prefiltered_by_query_words")]
    fn a_long_result_keeps_its_most_relevant_window(middle_lines: usize, retrievable: bool) {
        let state = tempfile::tempdir().unwrap();
        let (engine, _) = engine(&[NEEDLE], false);
        let retention = retention(state.path(), FeatureMode::Enforce, engine);
        let head = head(&[(long_result(middle_lines), retrievable)]);

        let plan = rank(&head, &retention, &CancelToken::none());

        let excerpt = &stripped(&head, plan.as_ref())[0];
        assert!(!stripped(&head, None)[0].contains(NEEDLE));
        assert!(excerpt.len() <= RETAINED_TOOL_RESULT_MAX_BYTES);
        assert!(excerpt.starts_with(HEAD));
        assert!(excerpt.ends_with(TAIL));
        assert!(excerpt.contains(NEEDLE));
        assert!(excerpt.contains(EXCERPT_MARKER));
        assert!(excerpt.contains(GAP_MARKER));
        assert_eq!(excerpt.contains(&output_id(0)), retrievable);
        assert_eq!(excerpt.contains(RETAIN_ID), retrievable);
    }

    #[test]
    fn a_long_result_without_a_relevant_window_keeps_its_head_and_tail() {
        let state = tempfile::tempdir().unwrap();
        let (engine, calls) = engine(&[], false);
        let retention = retention(state.path(), FeatureMode::Enforce, engine);
        let head = head(&[(long_result(SHORT_MIDDLE_LINES), true)]);

        assert!(rank(&head, &retention, &CancelToken::none()).is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_query_names_the_goal_the_request_and_open_todos() {
        let todos = [
            todo(FIRST_TODO, TodoStatus::Pending),
            todo(DONE_TODO, TodoStatus::Completed),
            todo(SECOND_TODO, TodoStatus::InProgress),
            todo(THIRD_TODO, TodoStatus::Pending),
            todo(FOURTH_TODO, TodoStatus::Pending),
        ];

        let query = retention_query(
            Some(GOAL),
            &todos,
            Some(&format!("{REQUEST} TOKEN={SECRET}")),
        )
        .unwrap();

        assert_eq!(query[GOAL_KEY], GOAL);
        assert!(query[REQUEST_KEY].as_str().unwrap().starts_with(REQUEST));
        assert!(!query.to_string().contains(SECRET));
        assert_eq!(
            query[OPEN_TODOS_KEY],
            json!([FIRST_TODO, SECOND_TODO, THIRD_TODO])
        );
    }

    #[test]
    fn an_escaped_query_still_fits_the_state() {
        let bloated = BLOATED_UNIT.repeat(ESCAPED_REPEATS);
        let todos = vec![todo(&bloated, TodoStatus::Pending); RESULT_COUNT];

        let query = retention_query(Some(&bloated), &todos, Some(&bloated)).unwrap();

        assert!(DecisionState::new(&query).is_ok());
        assert!(query.get(GOAL_KEY).is_some());
    }

    #[test_case(None, Some(" "); "a_blank_request")]
    #[test_case(None, None; "nothing")]
    fn nothing_to_judge_against_builds_no_query(goal: Option<&str>, request: Option<&str>) {
        assert!(retention_query(goal, &[], request).is_none());
    }
}
