use std::env;

use caudra_config::{AgentConfig, CompactionBuffer};
use caudra_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, StreamResponse, TokenUsage,
};
use caudra_storage::usage_ledger::LedgerPurpose;
use tracing::info;

use super::history::{History, remove_orphaned_tool_results};
use super::streaming::{StreamError, stream_with_retry};
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, DoneReason, EventSender, TurnCompleteEvent};

const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If the summary contains a todo list, restore it with todo_write and keep it updated. If you learned important project context during this session, consider saving it to memory before it's lost.";
const IMAGE_PLACEHOLDER: &str = "[image]";
const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
const KEEP_LAST_TOOL_RESULTS: usize = 3;
/// Byte cap for each retained compaction tool result; truncation stays on UTF-8 boundaries.
const RETAINED_TOOL_RESULT_MAX_BYTES: usize = 2_000;

fn normalize(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

pub(super) fn continue_message(config: &AgentConfig) -> String {
    match normalize(&config.post_compaction_instructions) {
        Some(extra) => format!("{CONTINUE_AFTER_COMPACT}\n\n{extra}"),
        None => CONTINUE_AFTER_COMPACT.to_string(),
    }
}

pub(super) async fn compact_history(
    provider: &dyn caudra_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
    config: &AgentConfig,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();
    let mut compaction_history: Vec<Message> = history.as_slice().to_vec();
    remove_orphaned_tool_results(&mut compaction_history);
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    strip_old_tool_results(&mut compaction_history);
    let summary_prompt = match normalize(&config.compaction_instructions) {
        Some(extra) => format!(
            "{}\n\nAdditional instructions:\n{extra}",
            crate::prompt::COMPACTION_USER
        ),
        None => crate::prompt::COMPACTION_USER.to_string(),
    };
    compaction_history.push(Message::user(summary_prompt));

    let empty_tools = serde_json::json!([]);
    let max_attempts = 3;
    let mut last_error = None;

    for attempt in 0..max_attempts {
        match stream_with_retry(
            provider,
            model,
            &compaction_history,
            crate::prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
        )
        .await
        {
            Ok(response) => {
                if attempt > 0 {
                    info!(
                        attempt,
                        "compaction succeeded after truncating oldest rounds"
                    );
                }
                return finish_compact(response, history, event_tx, compact_start, model);
            }
            Err(StreamError::Other(e)) if e.is_context_overflow() && attempt < max_attempts - 1 => {
                last_error = Some(e);
                truncate_oldest_round(&mut compaction_history);
            }
            Err(e) => return Err(e.into()),
        }
    }

    Err(last_error.unwrap())
}

fn finish_compact(
    mut response: StreamResponse,
    history: &mut History,
    event_tx: &EventSender,
    compact_start: std::time::Instant,
    model: &Model,
) -> Result<TokenUsage, AgentError> {
    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        provider: model.provider.to_string(),
        purpose: LedgerPurpose::Compaction,
        cost: model.billed_cost(&response.usage, false),
        context_size: Some(response.usage.output),
        context_window: model.context_window,
    })));

    // Swapping the history for a summary the model never wrote would throw the
    // session away for nothing.
    if response.message.first_text_content().is_none() {
        return Err(AgentError::EmptySummary);
    }

    response.message.retained_output_refs = retained_output_refs(history.as_slice());
    response.message.retained_subagent_ids = retained_subagent_ids(history.as_slice());
    response.message.is_compaction_summary = true;

    let new_history = vec![
        Message::user("What did we do so far?".into()),
        response.message,
    ];
    history.replace(new_history);
    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        "compaction completed"
    );

    Ok(response.usage)
}

fn retained_output_refs(messages: &[Message]) -> Vec<caudra_storage::tool_outputs::ToolOutputRef> {
    let mut retained_ids = std::collections::HashSet::new();
    messages
        .iter()
        .flat_map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        output_ref: Some(output_ref),
                        ..
                    } => Some(output_ref),
                    _ => None,
                })
                .chain(message.retained_output_refs.iter())
        })
        .filter(|output_ref| retained_ids.insert(output_ref.id))
        .cloned()
        .collect()
}

fn retained_subagent_ids(messages: &[Message]) -> Vec<String> {
    let mut retained = std::collections::HashSet::new();
    for message in messages {
        retained.extend(message.retained_subagent_ids.iter().cloned());
        for block in &message.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                retained.insert(id.clone());
            }
        }
    }
    let mut retained: Vec<_> = retained.into_iter().collect();
    retained.sort();
    retained
}

pub async fn compact(
    provider: &dyn caudra_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    config: &AgentConfig,
) -> Result<TokenUsage, AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, event_tx, &cancel, config).await?;
    if let Some(post) = normalize(&config.post_compaction_instructions) {
        history.push(Message::synthetic(post.to_string()));
    }

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        reason: DoneReason::EndTurn,
    })?;

    Ok(usage)
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, buffer: CompactionBuffer) -> bool {
    let usable = model
        .context_window
        .saturating_sub(buffer.resolve(model.context_window));
    usage.context_tokens() >= usable
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
        }
    }
}

fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| !block.is_thinking());
    }
}

fn strip_old_tool_results(messages: &mut [Message]) {
    let mut seen = 0;
    for message in messages.iter_mut().rev() {
        for block in message.content.iter_mut().rev() {
            if let ContentBlock::ToolResult {
                content,
                output_ref,
                ..
            } = block
            {
                if seen < KEEP_LAST_TOOL_RESULTS {
                    bound_retained_tool_result(content, output_ref.as_ref());
                } else {
                    *content = output_ref.as_ref().map_or_else(
                        || TOOL_RESULT_PLACEHOLDER.into(),
                        |output_ref| format!("[tool result omitted; output ID: {}]", output_ref.id),
                    );
                }
                seen += 1;
            }
        }
    }
}

fn bound_retained_tool_result(
    content: &mut String,
    output_ref: Option<&caudra_storage::tool_outputs::ToolOutputRef>,
) {
    if content.len() <= RETAINED_TOOL_RESULT_MAX_BYTES {
        return;
    }

    let marker = output_ref.map_or_else(
        || "[tool result truncated for compaction; middle omitted]".into(),
        |output_ref| {
            format!(
                "[tool result truncated for compaction; middle omitted; full output ID: {id}. Retain this ID in the resulting summary.]",
                id = output_ref.id,
            )
        },
    );
    let separator = "\n\n";
    let retained_budget = RETAINED_TOOL_RESULT_MAX_BYTES
        .saturating_sub(separator.len() * 2)
        .saturating_sub(marker.len());
    let head_end = utf8_prefix_end(content, retained_budget.div_ceil(2));
    let tail_start = utf8_suffix_start(content, retained_budget / 2);
    *content = format!(
        "{}{}{}{}{}",
        &content[..head_end],
        separator,
        marker,
        separator,
        &content[tail_start..]
    );
}

fn utf8_prefix_end(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn utf8_suffix_start(text: &str, max_bytes: usize) -> usize {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let removed_user = matches!(messages.remove(0).role, Role::User);
    if removed_user
        && messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
    }
    remove_orphaned_tool_results(messages);

    while messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
        remove_orphaned_tool_results(messages);
    }
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("CAUDRA_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use caudra_storage::id::{CaudraId, SessionRef};
    use caudra_storage::tool_outputs::ToolOutputRef;
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::AgentConfig;

    struct MockProvider {
        responses: Mutex<Vec<Result<StreamResponse, AgentError>>>,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Result<StreamResponse, AgentError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.requests.lock().unwrap().push(messages.to_vec());
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                responses.remove(0)
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn small_context_model(context_window: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
            ..Default::default()
        }
    }

    #[test]
    fn compact_replaces_history_with_summary() {
        smol::block_on(async {
            let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(MockProvider::new(
                vec![Ok(text_response(StopReason::EndTurn))],
            ));
            let model = default_model();
            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(vec![
                Message::user("first".into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "reply".into(),
                    }],
                    ..Default::default()
                },
            ]);

            compact(
                &*provider,
                &model,
                &mut history,
                &EventSender::new(raw_tx, 0),
                &AgentConfig::default(),
            )
            .await
            .unwrap();

            let msgs = history.as_slice();
            assert_eq!(msgs.len(), 2);
            assert!(matches!(msgs[0].role, Role::User));
            assert!(matches!(msgs[1].role, Role::Assistant));
        });
    }

    #[test_case(vec![] ; "no_content")]
    #[test_case(vec![ContentBlock::Text { text: " \n".into() }] ; "blank_text")]
    fn compact_keeps_history_when_summary_has_no_text(content: Vec<ContentBlock>) {
        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content,
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            })]);
            const KEPT: &str = "first";
            let mut history = History::new(vec![Message::user(KEPT.into())]);
            let (raw_tx, _rx) = flume::unbounded();

            let err = compact(
                &provider,
                &default_model(),
                &mut history,
                &EventSender::new(raw_tx, 0),
                &AgentConfig::default(),
            )
            .await
            .expect_err("empty summary must fail");

            assert!(matches!(err, AgentError::EmptySummary));
            assert_eq!(history.len(), 1);
            assert_eq!(history.as_slice()[0].user_text(), Some(KEPT));
        });
    }

    #[test]
    fn compact_applies_custom_instructions() {
        smol::block_on(async {
            const EXTRA: &str = "Record anything that belongs in plan.md";
            const POST: &str = "Re-read plan.md and agent.md";

            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![Message::user("work".into())]);
            let (raw_tx, _rx) = flume::unbounded();
            let config = AgentConfig {
                compaction_instructions: Some(EXTRA.into()),
                post_compaction_instructions: Some(POST.into()),
                ..Default::default()
            };

            compact(
                &provider,
                &default_model(),
                &mut history,
                &EventSender::new(raw_tx, 0),
                &config,
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            let summary_prompt = requests[0].last().unwrap();
            assert!(matches!(
                &summary_prompt.content[0],
                ContentBlock::Text { text }
                    if text.starts_with(crate::prompt::COMPACTION_USER) && text.ends_with(EXTRA)
            ));
            assert!(matches!(
                &history.as_slice().last().unwrap().content[0],
                ContentBlock::Text { text } if text == POST
            ));
        });
    }

    #[test_case(Some("  \n ".into()), None ; "whitespace_only_is_none")]
    #[test_case(Some("  keep plan.md ".into()), Some("keep plan.md") ; "trimmed")]
    fn normalize_instructions(raw: Option<String>, expected: Option<&str>) {
        assert_eq!(normalize(&raw), expected);
    }

    #[test]
    fn compact_preparation_removes_orphan_result_and_tool_image() {
        use std::sync::Arc;

        use caudra_providers::{ImageMediaType, ImageSource};

        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let image = ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8=")),
            };
            let mut orphan = Message {
                role: Role::User,
                content: vec![tool_result("orphan"), image.clone()],
                ..Default::default()
            };
            orphan.content.push(ContentBlock::Text {
                text: "keep text".into(),
            });
            let chat_image = Message {
                role: Role::User,
                content: vec![image],
                ..Default::default()
            };
            let mut history = History::new(vec![orphan, chat_image]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
                &AgentConfig::default(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            let request = &requests[0];
            assert!(
                !request
                    .iter()
                    .flat_map(|message| &message.content)
                    .any(|block| matches!(
                        block,
                        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                    ))
            );
            assert!(
                request.iter().flat_map(|message| &message.content).any(
                    |block| matches!(block, ContentBlock::Text { text } if text == "keep text")
                )
            );
            assert!(request.iter().flat_map(|message| &message.content).any(
                |block| matches!(block, ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
            ));
        });
    }

    #[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
    #[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
    #[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
    #[test_case(51_199,  0,       0,       0,      64_000,  false ; "small_window_below_scaled_threshold")]
    #[test_case(51_200,  0,       0,       0,      64_000,  true  ; "small_window_at_scaled_threshold")]
    fn overflow_detection(
        input: u32,
        cache_read: u32,
        cache_creation: u32,
        output: u32,
        ctx_window: u32,
        expected: bool,
    ) {
        let model = small_context_model(ctx_window);
        let usage = TokenUsage {
            input,
            output,
            cache_read,
            cache_creation,
        };
        assert_eq!(
            is_overflow(&usage, &model, AgentConfig::default().compaction_buffer),
            expected
        );
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 53_999, false ; "explicit_tokens_below")]
    #[test_case(CompactionBuffer::Tokens(10_000), 54_000, true  ; "explicit_tokens_honored")]
    #[test_case(CompactionBuffer::Percent(50),    32_000, true  ; "explicit_percent_at_threshold")]
    fn overflow_with_explicit_buffer(buffer: CompactionBuffer, input: u32, expected: bool) {
        let model = small_context_model(64_000);
        let usage = TokenUsage {
            input,
            ..Default::default()
        };
        assert_eq!(is_overflow(&usage, &model, buffer), expected);
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use caudra_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_thinking_removes_thinking_blocks() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("hmm".into(), Some("sig".into())),
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_old_tool_results_keeps_newest() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result 1".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old result 2".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep 1".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t4".into(),
                    content: "keep 2".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t5".into(),
                    content: "keep 3".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert_eq!(messages[0].content.len(), 6);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t1")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t2")
        );
        assert!(
            matches!(&messages[0].content[2], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 1" && tool_use_id == "t3")
        );
        assert!(
            matches!(&messages[0].content[3], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 2" && tool_use_id == "t4")
        );
        assert!(
            matches!(&messages[0].content[4], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 3" && tool_use_id == "t5")
        );
        assert!(
            matches!(&messages[0].content[5], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn strip_old_tool_results_uses_output_id_in_old_placeholder() {
        let output_ref = ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: 10_000,
            line_count: 100,
        };
        let mut messages = vec![Message {
            role: Role::User,
            content: (0..4)
                .map(|index| ContentBlock::ToolResult {
                    tool_use_id: format!("t{index}"),
                    content: format!("result {index}"),
                    is_error: false,
                    output_ref: (index == 0).then(|| output_ref.clone()),
                })
                .collect(),
            ..Default::default()
        }];

        strip_old_tool_results(&mut messages);

        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::ToolResult { content, output_ref: Some(actual), .. }
                if content.contains(&output_ref.id.to_string()) && actual == &output_ref
        ));
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::ToolResult { content, .. } if content == "result 1"
        ));
    }

    #[test]
    fn strip_old_tool_results_bounds_utf8_newest_result_by_bytes() {
        let output_ref = ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: 5_000,
            line_count: 1,
        };
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "newest".into(),
                content: format!("HEAD{}TAIL", "é".repeat(RETAINED_TOOL_RESULT_MAX_BYTES)),
                is_error: false,
                output_ref: Some(output_ref.clone()),
            }],
            ..Default::default()
        }];

        strip_old_tool_results(&mut messages);

        let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] else {
            unreachable!();
        };
        assert!(content.len() <= RETAINED_TOOL_RESULT_MAX_BYTES);
        assert!(content.starts_with("HEAD"));
        assert!(content.ends_with("TAIL"));
        assert!(content.contains(&output_ref.id.to_string()));
        assert!(content.contains("Retain this ID in the resulting summary"));
        assert!(!content.contains("tool_output_"));
    }

    #[test]
    fn strip_old_tool_results_bounds_single_huge_newest_result_without_ref() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "newest".into(),
                content: format!(
                    "head{}tail",
                    "x".repeat(RETAINED_TOOL_RESULT_MAX_BYTES * 100)
                ),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }];

        strip_old_tool_results(&mut messages);

        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::ToolResult { content, .. }
                if content.len() <= RETAINED_TOOL_RESULT_MAX_BYTES
                    && content.starts_with("head")
                    && content.ends_with("tail")
                    && content.contains("[tool result truncated for compaction; middle omitted]")
        ));
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, "bash", serde_json::json!({}))],
            ..Default::default()
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "output".into(),
            is_error: false,
            output_ref: None,
        }
    }

    #[test]
    fn compaction_carries_unique_current_and_prior_output_refs() {
        let current = ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: 10,
            line_count: 1,
        };
        let prior = ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: 20,
            line_count: 2,
        };
        let messages = [
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: "bounded".into(),
                    is_error: false,
                    output_ref: Some(current.clone()),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "summary".into(),
                }],
                retained_output_refs: vec![prior.clone(), current.clone()],
                ..Default::default()
            },
        ];

        assert_eq!(retained_output_refs(&messages), [current, prior]);
    }

    #[test]
    fn compaction_carries_tool_roots_and_prior_subagent_ids() {
        let messages = [
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use("task-direct", "task", serde_json::json!({})),
                    ContentBlock::tool_use("batch-root", "batch", serde_json::json!({})),
                    ContentBlock::tool_use("generic-root", "custom", serde_json::json!({})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "batch".into(),
                    content: "<task_metadata>\ntask_id: task-nested\n</task_metadata>".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message {
                retained_subagent_ids: vec!["task-prior".into(), "task-direct".into()],
                ..Default::default()
            },
        ];

        assert_eq!(
            retained_subagent_ids(&messages),
            ["batch-root", "generic-root", "task-direct", "task-prior"]
        );
    }

    #[track_caller]
    fn assert_tool_results_have_calls(messages: &[Message]) {
        for (index, message) in messages.iter().enumerate() {
            for block in &message.content {
                let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                    continue;
                };
                assert!(matches!(message.role, Role::User));
                assert!(index > 0);
                assert!(
                    messages[index - 1]
                        .tool_uses()
                        .any(|(id, _, _)| id == tool_use_id)
                );
            }
        }
    }

    #[test]
    fn compact_history_retries_without_reproduced_orphan() {
        smol::block_on(async {
            const TOOL_USE_ID: &str = "call_dMZDTpEfz2JxMvFbqFHua1Zy";

            let provider = MockProvider::new(vec![
                Err(AgentError::Api {
                    status: 413,
                    message: "prompt is too long".into(),
                }),
                Ok(text_response(StopReason::EndTurn)),
            ]);
            let mut history = History::new(vec![
                Message::user("request".into()),
                tool_use(TOOL_USE_ID),
                Message {
                    role: Role::User,
                    content: vec![tool_result(TOOL_USE_ID)],
                    ..Default::default()
                },
                Message::user("prompt".into()),
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
                &AgentConfig::default(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0]
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == TOOL_USE_ID)));
            assert!(
                !requests[1]
                    .iter()
                    .flat_map(|message| &message.content)
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            );
        });
    }

    #[test]
    fn compaction_keeps_observation_before_dependent_reply() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![
                Message::observation("[monitor] build failed".into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "I will fix it".into(),
                    }],
                    ..Default::default()
                },
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
                &AgentConfig::default(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            assert!(requests[0][0].is_observation());
            assert!(matches!(requests[0][1].role, Role::Assistant));
        });
    }

    #[test]
    fn truncate_oldest_round_preserves_text_beside_orphan() {
        let mut messages = vec![
            Message::user("request".into()),
            tool_use("expected"),
            Message {
                role: Role::User,
                content: vec![
                    tool_result("mismatched"),
                    ContentBlock::Text {
                        text: "keep me".into(),
                    },
                ],
                ..Default::default()
            },
            Message::user("prompt".into()),
        ];

        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);

        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == "keep me")
        );
        assert_tool_results_have_calls(&messages);
    }

    #[test]
    fn truncate_oldest_round_removes_single_user_message() {
        let mut messages = vec![
            Message::user("first".into()),
            Message::user("second".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "second"));
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_tool_pair() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_without_matching_tool_result() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message::user("no tool result".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "no tool result")
        );
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![Message::user("only".into())];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_plain_assistant() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
        // [User, Assistant(no tools), Assistant(tools), User(results)] drains 2,
        // leaving Assistant-first — keep draining until first is User.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "plain reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == "keep me")
        );
    }
}
