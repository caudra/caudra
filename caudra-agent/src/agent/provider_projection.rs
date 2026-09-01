use std::borrow::Cow;
use std::collections::HashMap;

use caudra_providers::{
    ContentBlock, Message, Model, ReasoningTransport, ResponsesReasoning, Role,
};
use serde_json::Value;

use crate::tools::{TOOL_OUTPUT_GREP_TOOL_NAME, TOOL_OUTPUT_READ_TOOL_NAME};

const PROTECTED_USER_TURNS: usize = 2;
const PROTECTED_OLD_RESULT_TOKENS: usize = 40_000;
const PRUNE_TRIGGER_TOKENS: usize = 20_000;
const BYTES_PER_TOKEN: usize = 4;
const READ_LIMIT: usize = 200;

struct Candidate {
    message_index: usize,
    block_index: usize,
    estimated_tokens: usize,
}

pub fn project<'a>(messages: &'a [Message], tools: &Value) -> Cow<'a, [Message]> {
    if !has_tool(tools, TOOL_OUTPUT_READ_TOOL_NAME) {
        return Cow::Borrowed(messages);
    }

    let protected_start = protected_turn_start(messages);
    if protected_start == 0 {
        return Cow::Borrowed(messages);
    }

    let tool_names: HashMap<&str, &str> = messages
        .iter()
        .flat_map(Message::tool_uses)
        .map(|(id, name, _)| (id, name))
        .collect();
    let eligible: Vec<Candidate> = messages[..protected_start]
        .iter()
        .enumerate()
        .flat_map(|(message_index, message)| {
            let tool_names = &tool_names;
            message
                .content
                .iter()
                .enumerate()
                .filter_map(move |(block_index, block)| {
                    let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: false,
                        output_ref: Some(_),
                    } = block
                    else {
                        return None;
                    };
                    if tool_names.get(tool_use_id.as_str()).is_some_and(|name| {
                        matches!(
                            *name,
                            TOOL_OUTPUT_READ_TOOL_NAME | TOOL_OUTPUT_GREP_TOOL_NAME
                        )
                    }) {
                        return None;
                    }
                    Some(Candidate {
                        message_index,
                        block_index,
                        estimated_tokens: estimate_tokens(content),
                    })
                })
        })
        .collect();

    let mut protected_tokens_remaining = PROTECTED_OLD_RESULT_TOKENS;
    let mut protecting = true;
    let mut candidates = Vec::new();
    for candidate in eligible.iter().rev() {
        if protecting && candidate.estimated_tokens <= protected_tokens_remaining {
            protected_tokens_remaining -= candidate.estimated_tokens;
        } else {
            protecting = false;
            candidates.push(candidate);
        }
    }
    let candidate_tokens = candidates
        .iter()
        .map(|candidate| candidate.estimated_tokens)
        .sum::<usize>();
    if candidate_tokens <= PRUNE_TRIGGER_TOKENS {
        return Cow::Borrowed(messages);
    }

    let mut projected = messages.to_vec();
    for candidate in candidates {
        let ContentBlock::ToolResult {
            content,
            output_ref: Some(output_ref),
            ..
        } = &mut projected[candidate.message_index].content[candidate.block_index]
        else {
            unreachable!("candidate shape changed while cloning history");
        };
        let id = output_ref.id;
        *content = format!(
            "[Old tool result pruned. Full output ID: {id}. Use tool_output_read(output_id=\"{id}\", offset=1, limit={READ_LIMIT}) or tool_output_grep(output_id=\"{id}\", pattern=\"...\").]"
        );
    }
    Cow::Owned(projected)
}

pub fn project_for_target<'a>(
    messages: &'a [Message],
    tools: &Value,
    model: &Model,
    transport: ReasoningTransport,
) -> Cow<'a, [Message]> {
    let projected = project(messages, tools);
    if !projected
        .iter()
        .any(|message| reasoning_requires_lowering(message, model, transport))
    {
        return projected;
    }

    let mut lowered = projected.into_owned();
    for message in &mut lowered {
        if reasoning_requires_lowering(message, model, transport) {
            lower_reasoning(message);
        }
    }
    Cow::Owned(lowered)
}

fn reasoning_requires_lowering(
    message: &Message,
    model: &Model,
    transport: ReasoningTransport,
) -> bool {
    if !matches!(message.role, Role::Assistant)
        || !message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            ) || matches!(
                block,
                ContentBlock::ToolUse {
                    thought_signature: Some(_),
                    ..
                }
            )
        })
    {
        return false;
    }
    message.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Thinking {
                interrupted: true,
                ..
            }
        ) || matches!(
            block,
            ContentBlock::Thinking {
                responses: Some(ResponsesReasoning {
                    encrypted_content,
                    ..
                }),
                ..
            } if encrypted_content.as_deref().is_none_or(str::is_empty)
        ) || (transport == ReasoningTransport::OpenAiResponses
            && matches!(
                block,
                ContentBlock::Thinking {
                    responses: None,
                    ..
                }
            ))
    }) || !message
        .reasoning_source
        .as_ref()
        .is_some_and(|source| source.matches(model, transport))
}

fn lower_reasoning(message: &mut Message) {
    let mut content = Vec::with_capacity(message.content.len());
    for block in message.content.drain(..) {
        match block {
            ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                content.push(ContentBlock::Text { text: thinking });
            }
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
            ContentBlock::ToolUse {
                id,
                name,
                input,
                thought_signature: _,
            } => content.push(ContentBlock::ToolUse {
                id,
                name,
                input,
                thought_signature: None,
            }),
            block => content.push(block),
        }
    }
    if content.is_empty() {
        content.push(ContentBlock::Text {
            text: caudra_providers::EMPTY_RESPONSE_MARKER.into(),
        });
    }
    message.content = content;
    message.reasoning_source = None;
}

fn has_tool(tools: &Value, name: &str) -> bool {
    tools.as_array().is_some_and(|definitions| {
        definitions
            .iter()
            .any(|definition| definition.get("name").and_then(Value::as_str) == Some(name))
    })
}

fn protected_turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| is_actual_user_turn(message))
        .nth(PROTECTED_USER_TURNS - 1)
        .map_or(0, |(index, _)| index)
}

fn is_actual_user_turn(message: &Message) -> bool {
    matches!(message.role, Role::User)
        && !message.is_observation()
        && message.display_text.as_deref() != Some("")
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

fn estimate_tokens(content: &str) -> usize {
    content.len().div_ceil(BYTES_PER_TOKEN)
}

#[cfg(test)]
mod tests {
    use caudra_storage::id::CaudraId;
    use caudra_storage::tool_outputs::ToolOutputRef;

    use super::*;

    fn anthropic_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn tools(include_read: bool) -> Value {
        if include_read {
            serde_json::json!([
                {"name": TOOL_OUTPUT_READ_TOOL_NAME},
                {"name": TOOL_OUTPUT_GREP_TOOL_NAME},
                {"name": "bash"}
            ])
        } else {
            serde_json::json!([{"name": "bash"}, {"name": TOOL_OUTPUT_GREP_TOOL_NAME}])
        }
    }

    fn output_ref(byte_count: usize) -> ToolOutputRef {
        ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count,
            line_count: 1,
        }
    }

    fn tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, name, serde_json::json!({}))],
            ..Default::default()
        }
    }

    fn result(id: &str, size: usize, is_error: bool, with_ref: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: id.repeat(size.div_ceil(id.len()))[..size].into(),
                is_error,
                output_ref: with_ref.then(|| output_ref(size)),
            }],
            ..Default::default()
        }
    }

    fn qualifying_history(candidate_size: usize) -> Vec<Message> {
        vec![
            Message::user("old request".into()),
            tool_use("candidate", "bash"),
            result("candidate", candidate_size, false, true),
            tool_use("retained", "bash"),
            result(
                "retained",
                PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN,
                false,
                true,
            ),
            Message::user("recent request one".into()),
            Message::user("recent request two".into()),
        ]
    }

    fn result_content<'a>(messages: &'a [Message], id: &str) -> &'a str {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => Some(content.as_str()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn exact_target_preserves_native_reasoning() {
        let model = anthropic_model();
        let mut message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::thinking(
                "private chain".into(),
                Some("signature".into()),
            )],
            ..Default::default()
        };
        message.reasoning_source = Some(caudra_providers::ReasoningSource::new(
            &model,
            ReasoningTransport::AnthropicMessages,
        ));

        let messages = [message];
        let projected = project_for_target(
            &messages,
            &tools(false),
            &model,
            ReasoningTransport::AnthropicMessages,
        );
        assert!(matches!(
            &projected[0].content[0],
            ContentBlock::Thinking {
                signature: Some(signature),
                ..
            } if signature == "signature"
        ));
    }

    #[test]
    fn different_target_lowers_visible_reasoning_and_drops_private_fields() {
        let source_model = anthropic_model();
        let target_model = Model::from_spec("openai/gpt-5.5").unwrap();
        let mut thinking = ContentBlock::thinking("visible summary".into(), Some("secret".into()));
        let ContentBlock::Thinking { responses, .. } = &mut thinking else {
            unreachable!();
        };
        *responses = Some(caudra_providers::ResponsesReasoning {
            item_id: "rs_1".into(),
            encrypted_content: Some("ciphertext".into()),
        });
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                thinking,
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
                ContentBlock::ToolUse {
                    id: "call".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                    thought_signature: Some("tool-secret".into()),
                },
            ],
            ..Default::default()
        };
        message.reasoning_source = Some(caudra_providers::ReasoningSource::new(
            &source_model,
            ReasoningTransport::AnthropicMessages,
        ));

        let messages = [message];
        let projected = project_for_target(
            &messages,
            &tools(false),
            &target_model,
            ReasoningTransport::OpenAiResponses,
        );
        assert!(matches!(
            &projected[0].content[..],
            [
                ContentBlock::Text { text },
                ContentBlock::ToolUse {
                    thought_signature: None,
                    ..
                }
            ] if text == "visible summary"
        ));
        assert!(projected[0].reasoning_source.is_none());
    }

    #[test]
    fn interrupted_reasoning_is_lowered_even_for_the_same_target() {
        let model = anthropic_model();
        let mut thinking = ContentBlock::thinking("partial".into(), Some("signature".into()));
        let ContentBlock::Thinking { interrupted, .. } = &mut thinking else {
            unreachable!();
        };
        *interrupted = true;
        let mut message = Message {
            role: Role::Assistant,
            content: vec![thinking],
            ..Default::default()
        };
        message.reasoning_source = Some(caudra_providers::ReasoningSource::new(
            &model,
            ReasoningTransport::AnthropicMessages,
        ));

        let messages = [message];
        let projected = project_for_target(
            &messages,
            &tools(false),
            &model,
            ReasoningTransport::AnthropicMessages,
        );
        assert!(matches!(
            &projected[0].content[..],
            [ContentBlock::Text { text }] if text == "partial"
        ));
    }

    #[test]
    fn responses_reasoning_without_usable_ciphertext_is_lowered() {
        let model = Model::from_spec("openai/gpt-5.4").unwrap();
        for encrypted_content in [None, Some(String::new())] {
            let mut message = Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    thinking: "visible summary".into(),
                    signature: None,
                    duration_ms: None,
                    interrupted: false,
                    responses: Some(ResponsesReasoning {
                        item_id: "rs_1".into(),
                        encrypted_content,
                    }),
                }],
                ..Default::default()
            };
            message.reasoning_source = Some(caudra_providers::ReasoningSource::new(
                &model,
                ReasoningTransport::OpenAiResponses,
            ));

            let messages = [message];
            let projected = project_for_target(
                &messages,
                &tools(false),
                &model,
                ReasoningTransport::OpenAiResponses,
            );

            assert!(matches!(
                &projected[0].content[..],
                [ContentBlock::Text { text }] if text == "visible summary"
            ));
        }
    }

    #[test]
    fn prunes_only_after_retention_and_trigger_thresholds() {
        let at_trigger = qualifying_history(PRUNE_TRIGGER_TOKENS * BYTES_PER_TOKEN);
        assert!(matches!(
            project(&at_trigger, &tools(true)),
            Cow::Borrowed(_)
        ));

        let over_trigger = qualifying_history(PRUNE_TRIGGER_TOKENS * BYTES_PER_TOKEN + 1);
        let projected = project(&over_trigger, &tools(true));
        assert!(matches!(&projected, Cow::Owned(_)));
        assert!(result_content(&projected, "candidate").starts_with("[Old tool result pruned."));
        let retained = "retained"
            .repeat((PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN).div_ceil("retained".len()));
        assert_eq!(
            result_content(&projected, "retained"),
            &retained[..PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN]
        );
    }

    #[test]
    fn replacement_keeps_ref_and_call_order_with_retrieval_instructions() {
        let history = qualifying_history(PRUNE_TRIGGER_TOKENS * BYTES_PER_TOKEN + 1);
        let original_ref = match &history[2].content[0] {
            ContentBlock::ToolResult { output_ref, .. } => output_ref.clone().unwrap(),
            _ => unreachable!(),
        };

        let projected = project(&history, &tools(true));
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            output_ref,
            ..
        } = &projected[2].content[0]
        else {
            unreachable!();
        };
        assert_eq!(tool_use_id, "candidate");
        assert_eq!(output_ref.as_ref(), Some(&original_ref));
        assert!(content.contains(&original_ref.id.to_string()));
        assert!(content.contains("offset=1, limit=200"));
        assert!(content.contains("tool_output_grep"));
        assert!(content.contains("pattern=\"...\""));
        assert_eq!(
            projected
                .iter()
                .flat_map(Message::tool_uses)
                .map(|(id, _, _)| id)
                .collect::<Vec<_>>(),
            ["candidate", "retained"]
        );
    }

    #[test]
    fn errors_missing_refs_and_retrieval_results_are_never_candidates() {
        let large = (PRUNE_TRIGGER_TOKENS + 1) * BYTES_PER_TOKEN;
        let mut history = vec![Message::user("old request".into())];
        for (id, name, is_error, with_ref) in [
            ("error", "bash", true, true),
            ("missing-ref", "bash", false, false),
            ("read-result", TOOL_OUTPUT_READ_TOOL_NAME, false, true),
            ("grep-result", TOOL_OUTPUT_GREP_TOOL_NAME, false, true),
        ] {
            history.push(tool_use(id, name));
            history.push(result(id, large, is_error, with_ref));
        }
        history.extend(qualifying_history(large).into_iter().skip(1));

        let projected = project(&history, &tools(true));
        assert!(result_content(&projected, "candidate").starts_with("[Old tool result pruned."));
        for id in ["error", "missing-ref", "read-result", "grep-result"] {
            assert!(!result_content(&projected, id).starts_with("[Old tool result pruned."));
        }
    }

    #[test]
    fn latest_actual_user_loops_ignore_tool_results_observations_and_padding() {
        let large = (PRUNE_TRIGGER_TOKENS + 1) * BYTES_PER_TOKEN;
        let mut history = qualifying_history(large);
        history.truncate(5);
        history.push(Message::user("recent request one".into()));
        history.push(tool_use("recent", "bash"));
        history.push(result("recent", large, false, true));
        history.push(Message::observation("background update".into()));
        history.push(Message::synthetic("continue".into()));
        history.push(Message::user("recent request two".into()));

        let projected = project(&history, &tools(true));
        assert!(result_content(&projected, "candidate").starts_with("[Old tool result pruned."));
        assert!(!result_content(&projected, "recent").starts_with("[Old tool result pruned."));
    }

    #[test]
    fn retrieval_absent_leaves_qualifying_history_borrowed() {
        let history = qualifying_history((PRUNE_TRIGGER_TOKENS + 1) * BYTES_PER_TOKEN);
        assert!(matches!(project(&history, &tools(false)), Cow::Borrowed(_)));
    }

    #[test]
    fn projection_does_not_mutate_canonical_serialization() {
        let history = qualifying_history((PRUNE_TRIGGER_TOKENS + 1) * BYTES_PER_TOKEN);
        let before = serde_json::to_vec(&history).unwrap();

        let projected = project(&history, &tools(true));

        assert_ne!(serde_json::to_vec(projected.as_ref()).unwrap(), before);
        assert_eq!(serde_json::to_vec(&history).unwrap(), before);
    }

    #[test]
    fn a_single_result_larger_than_the_reserve_is_pruned() {
        let history = vec![
            Message::user("old request".into()),
            tool_use("huge", "bash"),
            result(
                "huge",
                PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN + 1,
                false,
                true,
            ),
            Message::user("recent request one".into()),
            Message::user("recent request two".into()),
        ];

        let projected = project(&history, &tools(true));

        assert!(result_content(&projected, "huge").starts_with("[Old tool result pruned."));
    }

    #[test]
    fn an_oversized_result_ends_the_contiguous_protected_suffix() {
        let history = vec![
            Message::user("old request".into()),
            tool_use("older", "bash"),
            result(
                "older",
                (PRUNE_TRIGGER_TOKENS + 1) * BYTES_PER_TOKEN,
                false,
                true,
            ),
            tool_use("oversized", "bash"),
            result(
                "oversized",
                PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN + 1,
                false,
                true,
            ),
            tool_use("newer", "bash"),
            result("newer", BYTES_PER_TOKEN, false, true),
            Message::user("recent request one".into()),
            Message::user("recent request two".into()),
        ];

        let projected = project(&history, &tools(true));

        assert!(result_content(&projected, "older").starts_with("[Old tool result pruned."));
        assert!(result_content(&projected, "oversized").starts_with("[Old tool result pruned."));
        assert!(!result_content(&projected, "newer").starts_with("[Old tool result pruned."));
    }

    #[test]
    fn cjk_results_use_utf8_bytes_for_thresholds() {
        let cjk = "界".repeat((PRUNE_TRIGGER_TOKENS * BYTES_PER_TOKEN).div_ceil("界".len()) + 1);
        let history = vec![
            Message::user("old request".into()),
            tool_use("cjk", "bash"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "cjk".into(),
                    content: cjk,
                    is_error: false,
                    output_ref: Some(output_ref(100_000)),
                }],
                ..Default::default()
            },
            tool_use("retained", "bash"),
            result(
                "retained",
                PROTECTED_OLD_RESULT_TOKENS * BYTES_PER_TOKEN,
                false,
                true,
            ),
            Message::user("recent request one".into()),
            Message::user("recent request two".into()),
        ];

        let projected = project(&history, &tools(true));

        assert!(result_content(&projected, "cjk").starts_with("[Old tool result pruned."));
    }
}
