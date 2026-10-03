//! Extracts what the user asked for from a session: the messages they sent,
//! the answers they gave, and enough of each reply they were answering to
//! resolve a `go` or an `ignore docker`. One extractor serves `/extract`,
//! which streams the list into a modal, and compaction, which appends it to
//! the summary so nothing the user said is lost behind a seam.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

use caudra_providers::provider::Provider;
use caudra_providers::tokens::{cut_middle, estimate_tokens_cached};
use caudra_providers::{
    AgentError, ContentBlock, HistoryItem, HistoryItemKind, MIN_THINKING_BUDGET, Message, Model,
    ProviderEvent, RequestOptions, TokenUsage, UserOrigin,
};
use flume::Sender;
use serde_json::json;
use tracing::warn;

use super::compaction::COMPACTION_ANCHOR;
use crate::cancel::CancelToken;
use crate::tools::QUESTION_TOOL_NAME;
use crate::tools::native::question::asked_questions;
use crate::types::AskedQuestion;

/// Long enough for a list over a long session, short enough that a stalled
/// request never holds a compaction hostage.
const REQUIREMENTS_TIMEOUT: Duration = Duration::from_secs(120);
/// A list can run long, and a reasoning model draws its thinking budget from
/// the same pool, floored at [`MIN_THINKING_BUDGET`]; eight times the floor
/// leaves room for both.
pub const REQUIREMENTS_OUTPUT_TOKENS: u32 = MIN_THINKING_BUDGET * 8;
/// Share of the extract model's window the transcript may fill; the rest is
/// the prompt and the answer.
const TRANSCRIPT_WINDOW_PERCENT: u32 = 60;
/// Generous on purpose: nine replies in ten fit whole under the pair, so the
/// cut only touches the long ones, and the trimming reclaims the room on a
/// long session before any user turn is lost.
const AGENT_HEAD_TOKENS: u32 = 500;
const AGENT_TAIL_TOKENS: u32 = 1000;
/// A turn of at most this many words that opens with a steering word is the
/// user driving the agent, not telling it what to build.
const STEERING_MAX_WORDS: usize = 4;
const STEERING_LEADS: &[&str] = &[
    "go", "yes", "yep", "ok", "okay", "sure", "continue", "proceed", "done", "commit", "hi",
    "hello", "thanks",
];
/// Words a steering turn may open with ahead of the word that marks it.
const STEERING_PREFIXES: &[&str] = &["just", "now", "please"];
/// Heading under which the list is appended to a compaction summary. Distinct
/// from the summary's own second-level sections so it can be found and
/// carried forward as one block.
pub const REQUIREMENTS_MARKER: &str = "# User requirements";
const AGENT_LABEL: &str = "[agent]";
const PRIOR_LABEL: &str = "[earlier requirements]";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// A session as the extractor reads it, oldest first.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RequirementsInput {
    entries: Vec<Entry>,
    /// The reply after the last user turn, where a decision the user handed
    /// to the agent lands.
    closing: Option<String>,
    /// The section the last compaction summary carried, for when the
    /// transcript has to be cut and the oldest turns fall out.
    prior: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Turn {
        text: String,
        /// The reply the user was answering, cut to its head and tail.
        agent: Option<String>,
    },
    Answered {
        questions: Vec<AskedQuestion>,
        answers: String,
    },
}

/// One rendered entry: its agent block, droppable on its own, and its body.
struct Rendered {
    agent: Option<String>,
    body: String,
}

impl RequirementsInput {
    /// Reads user turns, answered questions, and the reply ahead of each turn
    /// out of a history, in order. The items may cross compaction seams:
    /// nothing here depends on the chain being a valid request.
    pub fn from_items(items: &[HistoryItem]) -> Self {
        let mut entries = Vec::new();
        let mut closing = None;
        let mut prior = None;
        let mut pending: HashMap<&str, Vec<AskedQuestion>> = HashMap::new();
        let mut last_agent: Option<&str> = None;
        for item in items {
            match &item.kind {
                HistoryItemKind::User {
                    text,
                    display_text,
                    origin: UserOrigin::Turn,
                    ..
                } => {
                    let Some(text) = turn_text(text, display_text.as_deref()) else {
                        continue;
                    };
                    if text == COMPACTION_ANCHOR || is_steering(text) {
                        continue;
                    }
                    entries.push(Entry::Turn {
                        text: text.to_owned(),
                        agent: last_agent.take().map(agent_block),
                    });
                }
                HistoryItemKind::AssistantText {
                    text,
                    is_compaction_summary: true,
                    ..
                } => {
                    prior = requirements_section(text).map(str::to_owned);
                }
                HistoryItemKind::AssistantText { text, .. } => {
                    if !text.trim().is_empty() {
                        last_agent = Some(text);
                    }
                }
                HistoryItemKind::ToolCall {
                    call_id,
                    name,
                    input,
                    ..
                } if name == QUESTION_TOOL_NAME => {
                    pending.insert(call_id, asked_questions(input));
                }
                HistoryItemKind::ToolResult {
                    call_id,
                    content,
                    is_error: false,
                    ..
                } => {
                    if let Some(questions) = pending.remove(call_id.as_str()) {
                        entries.push(Entry::Answered {
                            questions,
                            answers: content.trim().to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        if !entries.is_empty() {
            closing = last_agent.map(agent_block);
        }
        Self {
            entries,
            closing,
            prior,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn turns(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Turn { .. }))
            .count()
    }

    pub fn answers(&self) -> usize {
        self.entries.len() - self.turns()
    }

    /// The `# User requirements` block of the last compaction summary among
    /// the items, marker included.
    pub fn prior(&self) -> Option<&str> {
        self.prior.as_deref()
    }

    /// The rendered transcript, cut to fit the model's window. Agent blocks go
    /// first, oldest ahead, then the closing reply, and only then whole
    /// entries: what the user said outranks what they were answering. Once an
    /// entry is gone the prior section leads, so what fell out of the
    /// transcript is still on the list.
    fn transcript(&self, model: &Model) -> String {
        let budget = model
            .context_window
            .saturating_mul(TRANSCRIPT_WINDOW_PERCENT)
            / 100;
        let mut rendered: Vec<Rendered> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry.render(index + 1))
            .collect();
        let mut closing = self
            .closing
            .as_deref()
            .map(|reply| format!("{AGENT_LABEL}\n{reply}"));
        let mut total = rendered
            .iter()
            .map(|part| tokens(&part.body) + part.agent.as_deref().map_or(0, tokens))
            .chain(closing.as_deref().map(tokens))
            .fold(0u32, u32::saturating_add);

        for part in &mut rendered {
            if total <= budget {
                break;
            }
            if let Some(agent) = part.agent.take() {
                total -= tokens(&agent);
            }
        }
        if total > budget
            && let Some(reply) = closing.take()
        {
            total -= tokens(&reply);
        }

        let prior = self
            .prior
            .as_deref()
            .map(|section| format!("{PRIOR_LABEL}\n{section}"));
        let mut start = 0;
        if total > budget
            && let Some(prior) = &prior
        {
            total = total.saturating_add(tokens(prior));
        }
        while total > budget && start + 1 < rendered.len() {
            total -= tokens(&rendered[start].body);
            start += 1;
        }

        let mut blocks = Vec::with_capacity(rendered.len() * 2 + 3);
        if start > 0 {
            blocks.extend(prior);
            blocks.push(format!("[{start} earlier entries omitted]"));
        }
        for part in rendered.drain(start..) {
            blocks.extend(part.agent);
            blocks.push(part.body);
        }
        blocks.extend(closing);
        blocks.join("\n\n")
    }
}

fn tokens(text: &str) -> u32 {
    estimate_tokens_cached(text)
}

/// What the user saw themselves send, when the host recorded it apart from what
/// the model was sent. An empty display text marks a message hidden from the
/// user, which is then nothing they said.
fn turn_text<'a>(text: &'a str, display_text: Option<&'a str>) -> Option<&'a str> {
    let shown = match display_text {
        Some(display) if !display.is_empty() => display,
        Some(_) => return None,
        None => text,
    };
    let shown = shown.trim();
    (!shown.is_empty()).then_some(shown)
}

/// `go`, `commit this`, `yes`: the user moving the agent along. A quarter of
/// all turns are these, and none of them says what to build, so they are
/// dropped before the model sees them. Anything longer, or led by another
/// word, stays: `ignore docker` and `we're rather using uv` are two words each
/// and both are constraints.
fn is_steering(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() || words.len() > STEERING_MAX_WORDS {
        return false;
    }
    words
        .iter()
        .find(|word| !STEERING_PREFIXES.contains(&word.as_str()))
        .is_some_and(|lead| STEERING_LEADS.contains(&lead.as_str()))
}

/// The reply as the extractor sees it: whole when it fits under the head and
/// tail budget, otherwise its beginning and end with the gap counted.
fn agent_block(text: &str) -> String {
    match cut_middle(text, AGENT_HEAD_TOKENS, AGENT_TAIL_TOKENS) {
        Some(cut) => format!(
            "{}\n[… {} tokens omitted …]\n{}",
            cut.head, cut.omitted, cut.tail
        ),
        None => text.trim().to_owned(),
    }
}

impl Entry {
    fn render(&self, ordinal: usize) -> Rendered {
        match self {
            Entry::Turn { text, agent } => Rendered {
                agent: agent
                    .as_deref()
                    .map(|reply| format!("{AGENT_LABEL}\n{reply}")),
                body: format!("[user {ordinal}]\n{text}"),
            },
            Entry::Answered { questions, answers } => {
                let mut out = String::from("[question]\n");
                for question in questions {
                    let _ = writeln!(out, "{}: {}", question.header, question.question);
                    for option in &question.options {
                        let _ = writeln!(out, "  - {}: {}", option.label, option.description);
                    }
                }
                out.push_str("[answer]\n");
                out.push_str(answers);
                Rendered {
                    agent: None,
                    body: out,
                }
            }
        }
    }
}

/// What an extraction cost and what it produced. `text` is `None` when the
/// model answered with nothing usable; the spend is reported either way.
pub struct RequirementsOutcome {
    pub text: Option<String>,
    pub usage: TokenUsage,
}

/// Asks the model for the list. Text deltas and prefill progress reach
/// `events` as they stream, so a front end can draw the list while it is being
/// written and say how far the prompt has got before that. Errors only when the
/// request itself failed or stalled, which is the one case with no spend to
/// attribute.
///
/// The local channel stays between the provider and `events`: a provider whose
/// event sender is closed fails the request, so the caller's optional sender
/// cannot take its place.
///
/// No cache key: the request shares no prefix with the session and must not
/// claim its cache slot.
pub async fn extract(
    provider: &dyn Provider,
    model: &Model,
    input: &RequirementsInput,
    events: Option<&Sender<ProviderEvent>>,
    cancel: &CancelToken,
) -> Result<RequirementsOutcome, AgentError> {
    let prompt = crate::prompt::REQUIREMENTS_USER
        .replace(crate::prompt::TRANSCRIPT_SLOT, &input.transcript(model));
    let messages = [Message::user(prompt)];
    let tools = json!([]);
    let opts = RequestOptions::default().clamped(model);
    let (event_tx, event_rx) = flume::unbounded();
    let forwarder = smol::spawn({
        let events = events.cloned();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                if !matches!(
                    event,
                    ProviderEvent::TextDelta { .. } | ProviderEvent::PromptProgress { .. }
                ) {
                    continue;
                }
                if let Some(events) = &events
                    && events.send(event).is_err()
                {
                    return;
                }
            }
        }
    });

    let request = provider.stream_message(
        model,
        &messages,
        crate::prompt::REQUIREMENTS_SYSTEM,
        &tools,
        &event_tx,
        opts,
        None,
    );
    let response = futures_lite::future::race(
        futures_lite::future::race(request, async {
            cancel.cancelled().await;
            Err(AgentError::Cancelled)
        }),
        async {
            smol::Timer::after(REQUIREMENTS_TIMEOUT).await;
            Err(AgentError::Timeout {
                secs: REQUIREMENTS_TIMEOUT.as_secs(),
            })
        },
    )
    .await;
    drop(event_tx);
    forwarder.await;

    match response {
        Ok(response) => Ok(RequirementsOutcome {
            text: clean(&response_text(&response.message)),
            usage: response.usage,
        }),
        Err(error) => {
            warn!(%error, model = %model.id, "requirements extraction failed");
            Err(error)
        }
    }
}

fn response_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn clean(raw: &str) -> Option<String> {
    let stripped = strip_thinking(raw);
    let text = stripped.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Reasoning models emit the block inline when the API does not carry it
/// separately. An unterminated block means the answer never arrived, so
/// everything after it goes too.
fn strip_thinking(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find(THINK_OPEN) {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + THINK_OPEN.len()..];
        let Some(close) = after_open.find(THINK_CLOSE) else {
            return out;
        };
        rest = &after_open[close + THINK_CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// The `# User requirements` block of a compaction summary, marker included,
/// so a compaction whose extraction failed can carry the last one forward.
///
/// Only a marker opening its own line counts. A summary that merely quotes the
/// marker in prose would otherwise be carried forward as though it held the
/// section, and every later compaction would inherit that prose in place of the
/// requirements.
pub fn requirements_section(summary: &str) -> Option<&str> {
    summary
        .match_indices(REQUIREMENTS_MARKER)
        .find(|(start, _)| *start == 0 || summary.as_bytes()[*start - 1] == b'\n')
        .map(|(start, _)| summary[start..].trim_end())
}

#[cfg(test)]
mod tests {
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        AssistantTextState, CacheKey, CaudraId, ContentBlock, ModelInfo, Role, StreamResponse,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const FIRST: &str = "add a /extract command";
    const SECOND: &str = "default it to the fast model";
    const REPLY: &str = "I propose the Fast model for it. Shall I proceed?";
    const LATER_REPLY: &str = "Done: the command is wired to the Fast model.";
    const PRIOR_SECTION: &str = "# User requirements\n## Requirements\n- the earlier one";
    const HEADER: &str = "Model";
    const QUESTION: &str = "Which model?";
    const ANSWER: &str = "Model: fast";
    const CALL: &str = "call-1";
    const WIDE: u32 = 200_000;
    const LIST_DELTA: &str = "- ship the command";
    const PREFILL_PROCESSED: u32 = 1_200;
    const PREFILL_TOTAL: u32 = 4_000;
    const PREFILL_CACHE: u32 = 900;

    fn item(kind: HistoryItemKind) -> HistoryItem {
        HistoryItem {
            id: CaudraId::generate(),
            parent_id: None,
            supersedes: None,
            stands_for: None,
            group_id: CaudraId::generate(),
            kind,
        }
    }

    fn user(text: &str, display_text: Option<&str>, origin: UserOrigin) -> HistoryItem {
        item(HistoryItemKind::User {
            text: text.to_owned(),
            images: Vec::new(),
            display_text: display_text.map(str::to_owned),
            origin,
            steering: None,
            task_event: None,
            peer_event: None,
            workflow_event: None,
            standing_reminder: None,
            retained_output_refs: Vec::new(),
        })
    }

    fn turn(text: &str) -> HistoryItem {
        user(text, None, UserOrigin::Turn)
    }

    fn assistant(text: &str, is_compaction_summary: bool) -> HistoryItem {
        item(HistoryItemKind::AssistantText {
            text: text.to_owned(),
            state: AssistantTextState::Complete,
            retained_output_refs: Vec::new(),
            retained_subagent_ids: Vec::new(),
            is_compaction_summary,
        })
    }

    fn reasoning(text: &str) -> HistoryItem {
        item(HistoryItemKind::Reasoning {
            text: text.to_owned(),
            signature: None,
            redacted: false,
            interrupted: false,
            duration_ms: None,
            source: None,
            responses: None,
        })
    }

    fn question_call(call_id: &str) -> HistoryItem {
        item(HistoryItemKind::ToolCall {
            call_id: call_id.to_owned(),
            name: QUESTION_TOOL_NAME.to_owned(),
            input: json!({
                "questions": [{
                    "question": QUESTION,
                    "header": HEADER,
                    "options": [{"label": "fast", "description": "cheap"}],
                }]
            }),
            thought_signature: None,
            source: None,
        })
    }

    fn result(call_id: &str, content: &str, is_error: bool) -> HistoryItem {
        item(HistoryItemKind::ToolResult {
            call_id: call_id.to_owned(),
            content: content.to_owned(),
            is_error,
            output_ref: None,
            images: Vec::new(),
            refused_calls: Vec::new(),
        })
    }

    /// Reports one prefill frame and one text delta, which is what a caller
    /// watching the request has to be able to see.
    struct StreamingProvider;

    impl Provider for StreamingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            event_tx: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                event_tx.send(ProviderEvent::PromptProgress {
                    processed: PREFILL_PROCESSED,
                    total: PREFILL_TOTAL,
                    cache: PREFILL_CACHE,
                })?;
                event_tx.send(ProviderEvent::TextDelta {
                    text: LIST_DELTA.into(),
                })?;
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: LIST_DELTA.into(),
                        }],
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn extracted(events: Option<&Sender<ProviderEvent>>) -> Option<String> {
        smol::block_on(extract(
            &StreamingProvider,
            &model_with_window(WIDE),
            &RequirementsInput::from_items(&[turn(FIRST)]),
            events,
            &CancelToken::none(),
        ))
        .expect("the request succeeded")
        .text
    }

    /// The front end draws the list as it is written and says how far the
    /// prompt has got before that, so both have to survive the relay.
    #[test]
    fn extraction_relays_prefill_and_text_to_a_watching_caller() {
        let (tx, rx) = flume::unbounded();
        assert_eq!(extracted(Some(&tx)).as_deref(), Some(LIST_DELTA));
        drop(tx);

        let seen: Vec<ProviderEvent> = rx.drain().collect();
        assert!(
            matches!(
                seen.first(),
                Some(ProviderEvent::PromptProgress { processed, total, cache })
                    if *processed == PREFILL_PROCESSED
                        && *total == PREFILL_TOTAL
                        && *cache == PREFILL_CACHE
            ),
            "prefill leads the relay"
        );
        assert!(
            matches!(seen.get(1), Some(ProviderEvent::TextDelta { text }) if text == LIST_DELTA),
            "text follows it"
        );
    }

    /// Compaction extracts with nobody watching, and the provider's own sender
    /// has to stay live either way.
    #[test]
    fn extraction_without_a_watcher_still_produces_the_list() {
        assert_eq!(extracted(None).as_deref(), Some(LIST_DELTA));
    }

    fn model_with_window(context_window: u32) -> Model {
        let mut model = Model::from_spec("anthropic/claude-haiku-4-5").unwrap();
        model.context_window = context_window;
        model
    }

    /// Past the head and tail pair together, so the middle has to go.
    fn long_reply(index: usize) -> String {
        format!(
            "reply {index} {}",
            "word ".repeat((AGENT_HEAD_TOKENS + AGENT_TAIL_TOKENS) as usize * 2)
        )
    }

    #[test]
    fn only_user_turns_are_read() {
        let items = [
            turn(FIRST),
            user("observed", None, UserOrigin::Observation),
            user("synthetic", None, UserOrigin::Synthetic),
            user("mentioned", None, UserOrigin::Mention),
            turn(SECOND),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(input.turns(), 2);
        assert_eq!(input.answers(), 0);
        assert_eq!(
            input.transcript(&model_with_window(WIDE)),
            format!("[user 1]\n{FIRST}\n\n[user 2]\n{SECOND}")
        );
    }

    #[test_case("sent", Some("shown"), Some("shown") ; "display_text_wins")]
    #[test_case("sent", Some(""), None ; "empty_display_hides_the_turn")]
    #[test_case("sent", None, Some("sent") ; "no_display_reads_text")]
    #[test_case("  \n", None, None ; "blank_text_is_nothing")]
    fn a_turn_reads_what_the_user_saw(text: &str, display: Option<&str>, expected: Option<&str>) {
        assert_eq!(turn_text(text, display), expected);
    }

    #[test_case("go", true ; "go")]
    #[test_case("commit this", true ; "commit_this")]
    #[test_case("Commit it.", true ; "commit_with_punctuation")]
    #[test_case("just commit", true ; "prefixed_commit")]
    #[test_case("now commit the rest", true ; "prefixed_with_object")]
    #[test_case("yes", true ; "yes")]
    #[test_case("continue and then commit", true ; "continue_then_commit")]
    #[test_case("done", true ; "done")]
    #[test_case("ignore docker", false ; "two_word_constraint")]
    #[test_case("we're rather using `uv`", false ; "tool_preference")]
    #[test_case("go with this plan (you can now update memory as well)", false ; "long_go")]
    #[test_case("yes, continue with the refactor. make sure to follow SOLID principles.", false ; "yes_with_a_rule")]
    #[test_case("[red-mode active] now go", false ; "tagged_go_is_left_to_the_model")]
    #[test_case("", false ; "empty")]
    fn steering_is_told_apart_from_a_short_requirement(text: &str, expected: bool) {
        assert_eq!(is_steering(text), expected);
    }

    #[test]
    fn steering_turns_are_dropped_with_their_context() {
        let items = [
            turn(FIRST),
            assistant(REPLY, false),
            turn("go"),
            assistant(LATER_REPLY, false),
            turn(SECOND),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(input.turns(), 2);
        let transcript = input.transcript(&model_with_window(WIDE));
        assert!(!transcript.contains("go\n"), "{transcript}");
        assert!(
            !transcript.contains(REPLY),
            "the reply `go` answered goes with it"
        );
        assert!(transcript.contains(&format!(
            "{AGENT_LABEL}\n{LATER_REPLY}\n\n[user 2]\n{SECOND}"
        )));
    }

    #[test]
    fn the_reply_ahead_of_a_turn_is_rendered_before_it() {
        let items = [
            turn(FIRST),
            reasoning("thinking about it"),
            assistant("Let me look.", false),
            assistant(REPLY, false),
            turn(SECOND),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(
            input.transcript(&model_with_window(WIDE)),
            format!("[user 1]\n{FIRST}\n\n{AGENT_LABEL}\n{REPLY}\n\n[user 2]\n{SECOND}"),
            "only the last reply counts, and reasoning never does"
        );
    }

    #[test]
    fn the_reply_after_the_last_turn_closes_the_transcript() {
        let items = [turn(FIRST), assistant(LATER_REPLY, false)];

        let input = RequirementsInput::from_items(&items);

        assert!(
            input
                .transcript(&model_with_window(WIDE))
                .ends_with(&format!("{AGENT_LABEL}\n{LATER_REPLY}"))
        );
    }

    #[test]
    fn a_reply_with_no_turn_is_nothing_to_extract() {
        let input = RequirementsInput::from_items(&[assistant(REPLY, false)]);

        assert!(input.is_empty());
        assert_eq!(input.closing, None);
    }

    #[test]
    fn a_summary_is_the_prior_section_and_never_context() {
        let items = [
            turn(FIRST),
            assistant(
                format!("## Objective\n- x\n\n{PRIOR_SECTION}\n").as_str(),
                true,
            ),
            turn(SECOND),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(input.prior(), Some(PRIOR_SECTION));
        assert!(
            !input
                .transcript(&model_with_window(WIDE))
                .contains(AGENT_LABEL)
        );
    }

    #[test]
    fn the_compaction_anchor_is_skipped_whatever_its_origin() {
        let items = [turn(FIRST), turn(COMPACTION_ANCHOR)];

        assert_eq!(RequirementsInput::from_items(&items).turns(), 1);
    }

    #[test]
    fn a_long_reply_is_cut_to_its_ends() {
        let reply = long_reply(1);
        let items = [turn(FIRST), assistant(&reply, false), turn(SECOND)];

        let transcript = RequirementsInput::from_items(&items).transcript(&model_with_window(WIDE));

        assert!(transcript.contains("tokens omitted …]"), "{transcript}");
        assert!(transcript.contains("reply 1 word"), "the head survives");
        assert!(!transcript.contains(&reply), "the middle is gone");
    }

    #[test]
    fn an_answered_question_pairs_call_with_result() {
        let items = [
            turn(FIRST),
            question_call(CALL),
            result(CALL, ANSWER, false),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(input.answers(), 1);
        let transcript = input.transcript(&model_with_window(WIDE));
        assert!(transcript.contains(&format!("[user 1]\n{FIRST}")));
        assert!(transcript.contains(&format!(
            "[question]\n{HEADER}: {QUESTION}\n  - fast: cheap"
        )));
        assert!(transcript.ends_with(&format!("[answer]\n{ANSWER}")));
    }

    #[test]
    fn a_failed_or_unanswered_question_is_not_an_answer() {
        let items = [
            question_call(CALL),
            result(CALL, "cancelled", true),
            question_call("call-2"),
            result("other", ANSWER, false),
        ];

        let input = RequirementsInput::from_items(&items);

        assert!(input.is_empty());
    }

    #[test]
    fn over_budget_agent_blocks_go_before_any_user_turn() {
        let items: Vec<_> = (0..6)
            .flat_map(|index| {
                [
                    turn(&format!("requirement {index}")),
                    assistant(&long_reply(index), false),
                ]
            })
            .collect();
        let input = RequirementsInput::from_items(&items);

        // Room for two cut replies beside the six turns, not for six.
        let transcript = input.transcript(&model_with_window(6_000));

        assert!(
            !transcript.contains("earlier entries omitted"),
            "{transcript}"
        );
        assert!(
            !transcript.contains("reply 0 word"),
            "the oldest reply goes first"
        );
        assert!(
            transcript.contains("reply 5 word"),
            "the newest reply is kept"
        );
        for index in 0..6 {
            assert!(transcript.contains(&format!("requirement {index}")));
        }
    }

    #[test]
    fn a_transcript_over_budget_drops_the_oldest_entries() {
        let items: Vec<_> = (0..40)
            .map(|index| turn(&format!("requirement {index} {}", "x".repeat(400))))
            .collect();
        let input = RequirementsInput::from_items(&items);

        let transcript = input.transcript(&model_with_window(2_000));

        assert!(transcript.starts_with('['));
        assert!(transcript.contains("earlier entries omitted]"));
        assert!(!transcript.contains(PRIOR_LABEL), "nothing to seed with");
        assert!(!transcript.contains("[user 1]\n"));
        assert!(transcript.contains("[user 40]\n"));
    }

    #[test]
    fn a_cut_transcript_leads_with_the_prior_section() {
        let mut items = vec![assistant(PRIOR_SECTION, true)];
        items
            .extend((0..40).map(|index| turn(&format!("requirement {index} {}", "x".repeat(400)))));
        let input = RequirementsInput::from_items(&items);

        let transcript = input.transcript(&model_with_window(2_000));

        assert!(
            transcript.starts_with(&format!("{PRIOR_LABEL}\n{PRIOR_SECTION}\n\n[")),
            "{transcript}"
        );
    }

    #[test]
    fn a_transcript_within_budget_keeps_everything_and_no_seed() {
        let items = [assistant(PRIOR_SECTION, true), turn(FIRST), turn(SECOND)];
        let input = RequirementsInput::from_items(&items);

        let transcript = input.transcript(&model_with_window(WIDE));

        assert_eq!(
            transcript,
            format!("[user 1]\n{FIRST}\n\n[user 2]\n{SECOND}")
        );
    }

    #[test_case("## Requirements\n- one", Some("## Requirements\n- one") ; "plain")]
    #[test_case("  <think>weighing</think>\n## Requirements\n- one\n", Some("## Requirements\n- one") ; "strips_thinking")]
    #[test_case("<think>never finished", None ; "unterminated_thinking_yields_nothing")]
    #[test_case("  \n", None ; "blank_yields_nothing")]
    fn clean_keeps_the_answer_only(raw: &str, expected: Option<&str>) {
        assert_eq!(clean(raw).as_deref(), expected);
    }

    /// The labels the renderer writes are the ones the prompt explains.
    #[test_case(AGENT_LABEL ; "agent_block")]
    #[test_case(PRIOR_LABEL ; "prior_block")]
    #[test_case("[user N]" ; "user_block")]
    #[test_case("(was: " ; "supersession_note")]
    fn the_prompts_explain_what_the_transcript_carries(needle: &str) {
        assert!(
            crate::prompt::REQUIREMENTS_SYSTEM.contains(needle),
            "{needle}"
        );
        assert!(
            crate::prompt::REQUIREMENTS_USER.contains(needle),
            "{needle}"
        );
    }

    #[test_case("## Objective\n- x\n\n# User requirements\n## Requirements\n- one\n\n", Some("# User requirements\n## Requirements\n- one") ; "found")]
    #[test_case("## Objective\n- x\n", None ; "absent")]
    #[test_case("# User requirements\n- one\n", Some("# User requirements\n- one") ; "at_the_very_start")]
    fn the_section_is_found_by_its_marker(summary: &str, expected: Option<&str>) {
        assert_eq!(requirements_section(summary), expected);
    }

    /// A session about this feature discusses the marker, and a summary of it
    /// quotes the marker in prose. Matching that would carry the prose forward
    /// as the requirements and every later compaction would inherit it.
    #[test_case("- the const is `REQUIREMENTS_MARKER = \"# User requirements\"`\n" ; "quoted_in_a_bullet")]
    #[test_case("The marker is # User requirements, appended verbatim.\n" ; "quoted_mid_sentence")]
    #[test_case("## User requirements are extracted per session\n" ; "deeper_heading")]
    fn a_marker_that_does_not_open_a_line_is_not_a_section(summary: &str) {
        assert_eq!(requirements_section(summary), None);
    }

    /// Prose first, a real section after: the section still wins, because the
    /// search keeps looking rather than stopping at the first textual match.
    #[test]
    fn a_real_section_is_found_past_prose_that_quotes_the_marker() {
        let summary = "- we append `# User requirements` verbatim\n\n# User requirements\n- one\n";
        assert_eq!(
            requirements_section(summary),
            Some("# User requirements\n- one")
        );
    }
}
