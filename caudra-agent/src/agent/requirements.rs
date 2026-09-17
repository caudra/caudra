//! Extracts what the user asked for from their side of a session: the messages
//! they sent and the answers they gave. One extractor serves `/extract`, which
//! streams the list into a modal, and compaction, which appends it to the
//! summary so nothing the user said is lost behind a seam.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

use caudra_providers::provider::Provider;
use caudra_providers::{
    AgentError, ContentBlock, HistoryItem, HistoryItemKind, MIN_THINKING_BUDGET, Message, Model,
    ProviderEvent, RequestOptions, TokenUsage, UserOrigin,
};
use flume::Sender;
use serde_json::json;
use tracing::warn;

use super::run::estimate_message_tokens;
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
/// Heading under which the list is appended to a compaction summary. Distinct
/// from the summary's own second-level sections so it can be found and
/// carried forward as one block.
pub const REQUIREMENTS_MARKER: &str = "# User requirements";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// The user's side of a session, oldest first.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RequirementsInput {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Turn(String),
    Answered {
        questions: Vec<AskedQuestion>,
        answers: String,
    },
}

impl RequirementsInput {
    /// Reads user turns and answered questions out of a history, in order. The
    /// items may cross compaction seams: nothing here depends on the chain
    /// being a valid request.
    pub fn from_items(items: &[HistoryItem]) -> Self {
        let mut entries = Vec::new();
        let mut pending: HashMap<&str, Vec<AskedQuestion>> = HashMap::new();
        for item in items {
            match &item.kind {
                HistoryItemKind::User {
                    text,
                    display_text,
                    origin: UserOrigin::Turn,
                    ..
                } => {
                    if let Some(text) = turn_text(text, display_text.as_deref()) {
                        entries.push(Entry::Turn(text.to_owned()));
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
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn turns(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Turn(_)))
            .count()
    }

    pub fn answers(&self) -> usize {
        self.entries.len() - self.turns()
    }

    /// The rendered transcript, cut to fit the model's window by dropping the
    /// oldest entries first: what the user said last is what the work is on.
    fn transcript(&self, model: &Model) -> String {
        let budget = model
            .context_window
            .saturating_mul(TRANSCRIPT_WINDOW_PERCENT)
            / 100;
        let rendered: Vec<String> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry.render(index + 1))
            .collect();
        let mut start = 0;
        while start + 1 < rendered.len()
            && estimate_message_tokens(&[Message::user(rendered[start..].join("\n\n"))]) > budget
        {
            start += 1;
        }
        let mut transcript = rendered[start..].join("\n\n");
        if start > 0 {
            transcript.insert_str(0, &format!("[{start} earlier entries omitted]\n\n"));
        }
        transcript
    }
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

impl Entry {
    fn render(&self, ordinal: usize) -> String {
        match self {
            Entry::Turn(text) => format!("[user {ordinal}]\n{text}"),
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
                out
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

/// Asks the model for the list. Text deltas reach `deltas` as they stream, so a
/// front end can draw the list while it is being written. Errors only when the
/// request itself failed or stalled, which is the one case with no spend to
/// attribute.
///
/// No cache key: the request shares no prefix with the session and must not
/// claim its cache slot.
pub async fn extract(
    provider: &dyn Provider,
    model: &Model,
    input: &RequirementsInput,
    deltas: Option<&Sender<String>>,
    cancel: &CancelToken,
) -> Result<RequirementsOutcome, AgentError> {
    let prompt = crate::prompt::REQUIREMENTS_USER
        .replace(crate::prompt::TRANSCRIPT_SLOT, &input.transcript(model));
    let messages = [Message::user(prompt)];
    let tools = json!([]);
    let opts = RequestOptions::default().clamped(model);
    let (event_tx, event_rx) = flume::unbounded();
    let forwarder = smol::spawn({
        let deltas = deltas.cloned();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let ProviderEvent::TextDelta { text } = event else {
                    continue;
                };
                if let Some(deltas) = &deltas
                    && deltas.send(text).is_err()
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
pub fn requirements_section(summary: &str) -> Option<&str> {
    summary
        .find(REQUIREMENTS_MARKER)
        .map(|start| summary[start..].trim_end())
}

#[cfg(test)]
mod tests {
    use caudra_providers::CaudraId;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const FIRST: &str = "add a /extract command";
    const SECOND: &str = "default it to the fast model";
    const HEADER: &str = "Model";
    const QUESTION: &str = "Which model?";
    const ANSWER: &str = "Model: fast";
    const CALL: &str = "call-1";

    fn item(kind: HistoryItemKind) -> HistoryItem {
        HistoryItem {
            id: CaudraId::generate(),
            parent_id: None,
            supersedes: None,
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
        })
    }

    fn model_with_window(context_window: u32) -> Model {
        let mut model = Model::from_spec("anthropic/claude-haiku-4-5").unwrap();
        model.context_window = context_window;
        model
    }

    #[test]
    fn only_user_turns_are_read() {
        let items = [
            user(FIRST, None, UserOrigin::Turn),
            user("observed", None, UserOrigin::Observation),
            user("synthetic", None, UserOrigin::Synthetic),
            user("mentioned", None, UserOrigin::Mention),
            user(SECOND, None, UserOrigin::Turn),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(
            input.entries,
            vec![Entry::Turn(FIRST.into()), Entry::Turn(SECOND.into())]
        );
        assert_eq!(input.turns(), 2);
        assert_eq!(input.answers(), 0);
    }

    #[test_case("sent", Some("shown"), Some("shown") ; "display_text_wins")]
    #[test_case("sent", Some(""), None ; "empty_display_hides_the_turn")]
    #[test_case("sent", None, Some("sent") ; "no_display_reads_text")]
    #[test_case("  \n", None, None ; "blank_text_is_nothing")]
    fn a_turn_reads_what_the_user_saw(text: &str, display: Option<&str>, expected: Option<&str>) {
        assert_eq!(turn_text(text, display), expected);
    }

    #[test]
    fn an_answered_question_pairs_call_with_result() {
        let items = [
            user(FIRST, None, UserOrigin::Turn),
            question_call(CALL),
            result(CALL, ANSWER, false),
        ];

        let input = RequirementsInput::from_items(&items);

        assert_eq!(input.answers(), 1);
        let transcript = input.transcript(&model_with_window(200_000));
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
    fn a_transcript_over_budget_drops_the_oldest_entries() {
        let items: Vec<_> = (0..40)
            .map(|index| {
                user(
                    &format!("requirement {index} {}", "x".repeat(400)),
                    None,
                    UserOrigin::Turn,
                )
            })
            .collect();
        let input = RequirementsInput::from_items(&items);

        let transcript = input.transcript(&model_with_window(2_000));

        assert!(transcript.starts_with('['));
        assert!(transcript.contains("earlier entries omitted]"));
        assert!(!transcript.contains("[user 1]\n"));
        assert!(transcript.contains("[user 40]\n"));
    }

    #[test]
    fn a_transcript_within_budget_keeps_everything() {
        let items = [
            user(FIRST, None, UserOrigin::Turn),
            user(SECOND, None, UserOrigin::Turn),
        ];
        let input = RequirementsInput::from_items(&items);

        let transcript = input.transcript(&model_with_window(200_000));

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

    #[test_case("## Objective\n- x\n\n# User requirements\n## Requirements\n- one\n\n", Some("# User requirements\n## Requirements\n- one") ; "found")]
    #[test_case("## Objective\n- x\n", None ; "absent")]
    fn the_section_is_found_by_its_marker(summary: &str, expected: Option<&str>) {
        assert_eq!(requirements_section(summary), expected);
    }
}
