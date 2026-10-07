use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::types::{append_tool_input, parse_tool_input};
use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{HttpClient, Request};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::WireRequest;
use crate::providers::ResolvedAuth;
use crate::{
    AgentError, CacheKey, ContentBlock, Message, ProviderEvent, ResponsesReasoning, Role,
    StopReason, StreamResponse, ThinkingConfig, TokenUsage,
};

pub(crate) const RESPONSES_PATH: &str = "/responses";
const NO_BASE_URL: &str = "Responses API requires a base_url in auth";
pub(crate) const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
pub(crate) const PROMPT_CACHE_KEY_FIELD: &str = "prompt_cache_key";
pub(crate) const CACHE_BREAKPOINT_FIELD: &str = "prompt_cache_breakpoint";
pub(crate) const INSTRUCTIONS_FIELD: &str = "instructions";
pub(crate) const DEVELOPER_ROLE: &str = "developer";
const EXPLICIT_BREAKPOINT_MODE: &str = "explicit";

/// Routes the request to the cache holding this conversation. The field is
/// part of OpenAI's Responses and Chat Completions APIs alike.
pub(crate) fn apply_prompt_cache_key(body: &mut Value, cache_key: Option<&CacheKey>) {
    if let Some(cache_key) = cache_key {
        body[PROMPT_CACHE_KEY_FIELD] = json!(cache_key.as_str());
    }
}

/// Closes the system prompt with an explicit cache breakpoint. Implicit caching
/// writes through the latest message, so a conversation that shares system and
/// tools but opens with a different user turn (a new session, a subagent)
/// would miss the whole prefix. Top-level `instructions` cannot carry the mark,
/// so the prompt moves into a developer message. GPT-5.6 and later only:
/// earlier models reject the field.
pub(crate) fn apply_system_breakpoint(body: &mut Value) {
    if body[INSTRUCTIONS_FIELD].as_str().is_none_or(str::is_empty) {
        return;
    }
    let Some(system) = body
        .as_object_mut()
        .and_then(|body| body.remove(INSTRUCTIONS_FIELD))
    else {
        return;
    };
    let developer = json!({
        "type": "message",
        "role": DEVELOPER_ROLE,
        "content": [{
            "type": "input_text",
            "text": system,
            CACHE_BREAKPOINT_FIELD: { "mode": EXPLICIT_BREAKPOINT_MODE },
        }],
    });
    if let Some(input) = body["input"].as_array_mut() {
        input.insert(0, developer);
    }
}

pub(crate) fn build_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
) -> Value {
    let input = convert_input(messages);
    let wire_tools = convert_tools(tools);

    let mut body = json!({
        "model": model.id,
        INSTRUCTIONS_FIELD: system,
        "input": input,
        "include": [ENCRYPTED_REASONING],
        "stream": true,
        "store": false,
    });
    if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
        body["tools"] = wire_tools;
    }
    body
}

pub(crate) fn apply_responses_reasoning(
    body: &mut Value,
    thinking: &ThinkingConfig,
    model: &Model,
) {
    body["reasoning"] = json!({ "summary": "auto" });
    if let Some(effort) = thinking.effort_str(model) {
        body["reasoning"]["effort"] = json!(effort);
    }
}

pub(crate) fn convert_input(messages: &[Message]) -> Value {
    let mut input = Vec::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_text", "text": text}]
                            }));
                        }
                        ContentBlock::Image { source } => {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_image", "image_url": source.to_data_url()}]
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }
            }
            Role::Assistant => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();
                let mut reasoning_items: Vec<(String, String, Vec<&str>)> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::Thinking {
                            thinking,
                            responses:
                                Some(ResponsesReasoning {
                                    item_id,
                                    encrypted_content: Some(encrypted_content),
                                }),
                            ..
                        } if !encrypted_content.is_empty() => {
                            if let Some((_, stored_encrypted, summaries)) = reasoning_items
                                .iter_mut()
                                .find(|(stored_id, _, _)| stored_id == item_id)
                            {
                                encrypted_content.clone_into(stored_encrypted);
                                if !thinking.is_empty() {
                                    summaries.push(thinking);
                                }
                            } else {
                                reasoning_items.push((
                                    item_id.clone(),
                                    encrypted_content.clone(),
                                    if thinking.is_empty() {
                                        Vec::new()
                                    } else {
                                        vec![thinking]
                                    },
                                ));
                            }
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_calls.push((id, name, input));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                for (id, encrypted_content, summaries) in reasoning_items {
                    input.push(json!({
                        "type": "reasoning",
                        "id": id,
                        "summary": summaries
                            .into_iter()
                            .map(|text| json!({"type": "summary_text", "text": text}))
                            .collect::<Vec<_>>(),
                        "encrypted_content": encrypted_content,
                    }));
                }

                if !text_parts.is_empty() {
                    let joined = text_parts.join("");
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": joined}]
                    }));
                }

                for (id, name, args) in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args.to_string(),
                    }));
                }
            }
        }
    }

    Value::Array(input)
}

pub(crate) fn convert_tools(anthropic_tools: &Value) -> Value {
    let Some(tools) = anthropic_tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "name": t.get("name")?,
                    "description": t.get("description")?,
                    "parameters": t.get("input_schema")?,
                    "strict": false,
                }))
            })
            .collect(),
    )
}

/// Where a Responses turn posts, for the send and the dry run alike.
pub(crate) fn responses_url(auth: &ResolvedAuth) -> Result<String, AgentError> {
    let base = auth.base_url.as_deref().ok_or_else(|| AgentError::Config {
        message: NO_BASE_URL.into(),
    })?;
    Ok(format!("{base}{RESPONSES_PATH}"))
}

pub(crate) async fn do_stream(
    client: &HttpClient,
    model: &crate::model::Model,
    wire: &WireRequest,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let request = auth
        .configure_request(
            Request::builder()
                .method(wire.method)
                .uri(&wire.url)
                .header("content-type", "application/json")
                .header("user-agent", super::super::user_agent()),
        )
        .body(serde_json::to_vec(&wire.body)?)?;

    debug!(
        model = %model.id,
        provider = "OpenAI Coding Plan",
        "sending Responses API request"
    );

    let response = client.send_async(request).await?;
    let status = response.status().as_u16();

    if status == 200 {
        parse_sse(
            BufReader::new(response.into_body()),
            event_tx,
            stream_timeout,
        )
        .await
    } else {
        Err(AgentError::from_response(response).await)
    }
}

struct ToolAccumulator {
    output_index: u64,
    call_id: String,
    name: String,
    arguments: String,
    incomplete: bool,
}

struct ReasoningAccumulator {
    item_id: Option<String>,
    summary_index: u64,
    text: String,
    encrypted_content: Option<String>,
}

fn reasoning_accumulator<'a>(
    reasoning: &'a mut Vec<ReasoningAccumulator>,
    item_id: Option<&str>,
    summary_index: u64,
) -> &'a mut ReasoningAccumulator {
    if let Some(position) = reasoning
        .iter()
        .position(|part| part.item_id.as_deref() == item_id && part.summary_index == summary_index)
    {
        return &mut reasoning[position];
    }
    reasoning.push(ReasoningAccumulator {
        item_id: item_id.map(ToOwned::to_owned),
        summary_index,
        text: String::new(),
        encrypted_content: None,
    });
    reasoning.last_mut().unwrap()
}

pub(crate) async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();

    let mut text = String::new();
    let mut reasoning: Vec<ReasoningAccumulator> = Vec::new();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut response_complete = false;
    let mut is_first_content = true;
    let mut deadline = Instant::now() + stream_timeout;
    let mut current_event = String::new();

    while let Some(line) =
        crate::providers::next_sse_line(&mut lines, &mut deadline, stream_timeout).await?
    {
        if let Some(event_type) = line.strip_prefix("event:") {
            current_event = event_type.trim().to_string();
            continue;
        }

        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };

        if current_event == "error" {
            if let Ok(ev) = serde_json::from_str::<crate::providers::SseErrorPayload>(data) {
                warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error in stream");
                return Err(ev.into_agent_error());
            }
            let parsed: Value = serde_json::from_str(data).unwrap_or_default();
            let message = parsed["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            return Err(AgentError::api(500, message));
        }

        let parsed_event = if current_event.is_empty() {
            serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|value| value["type"].as_str().map(ToOwned::to_owned))
                .unwrap_or_default()
        } else {
            current_event.clone()
        };

        match parsed_event.as_str() {
            "response.output_text.delta" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = parsed["delta"].as_str()
                    && !delta.is_empty()
                {
                    let delta = if is_first_content {
                        is_first_content = false;
                        delta.trim_start().to_string()
                    } else {
                        delta.to_string()
                    };
                    if !delta.is_empty() {
                        text.push_str(&delta);
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: delta })
                            .await?;
                    }
                }
            }

            "response.output_item.added" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let item = &parsed["item"];
                let output_index = parsed["output_index"]
                    .as_u64()
                    .unwrap_or(tool_accumulators.len() as u64);
                if item["type"].as_str() == Some("reasoning") {
                    let item_id = item["id"].as_str();
                    if reasoning.iter().any(|part| !part.text.is_empty())
                        && !reasoning.iter().any(|part| {
                            part.item_id.as_deref() == item_id && part.summary_index == 0
                        })
                    {
                        event_tx.send_async(ProviderEvent::ThinkingBoundary).await?;
                    }
                    let part = reasoning_accumulator(&mut reasoning, item_id, 0);
                    part.encrypted_content = item["encrypted_content"]
                        .as_str()
                        .filter(|content| !content.is_empty())
                        .map(ToOwned::to_owned);
                } else if item["type"].as_str() == Some("function_call") {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    if !name.is_empty() && !call_id.is_empty() {
                        event_tx
                            .send_async(ProviderEvent::ToolUseStart {
                                id: call_id.clone(),
                                name: name.clone(),
                                source_ordinal: None,
                            })
                            .await?;
                    }
                    tool_accumulators.push(ToolAccumulator {
                        output_index,
                        call_id,
                        name,
                        arguments: String::new(),
                        incomplete: false,
                    });
                }
            }

            "response.function_call_arguments.delta" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let delta: Cow<'_, str> = if let Some(s) = parsed["delta"].as_str() {
                    Cow::Borrowed(s)
                } else if let Some(obj) = parsed["delta"].as_object() {
                    Cow::Owned(serde_json::to_string(obj).unwrap_or_default())
                } else {
                    Cow::Borrowed("")
                };
                if !delta.is_empty() {
                    let acc = if let Some(idx) = parsed["output_index"].as_u64() {
                        tool_accumulators.iter_mut().find(|a| a.output_index == idx)
                    } else {
                        tool_accumulators.last_mut()
                    };
                    if let Some(acc) = acc {
                        append_tool_input(&mut acc.arguments, &delta);
                        let id = acc.call_id.clone();
                        event_tx
                            .send_async(ProviderEvent::ToolInputDelta {
                                id,
                                delta: delta.into_owned(),
                            })
                            .await?;
                    }
                }
            }

            "response.function_call_arguments.done" => {
                let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let index = parsed["output_index"].as_u64();
                let acc = tool_accumulators
                    .iter_mut()
                    .find(|acc| Some(acc.output_index) == index);
                if let Some(acc) = acc {
                    if let Some(arguments) = parsed["arguments"].as_str() {
                        acc.arguments.clear();
                        append_tool_input(&mut acc.arguments, arguments);
                    }
                    if !acc.call_id.is_empty() && !acc.name.is_empty() {
                        let (input, invalid_input) = parse_tool_input(&acc.arguments, false);
                        event_tx
                            .send_async(ProviderEvent::ToolInputReady {
                                id: acc.call_id.clone(),
                                name: acc.name.clone(),
                                input,
                                invalid_input,
                            })
                            .await?;
                    }
                }
            }

            "response.in_progress" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(pp) = parsed.get("prompt_progress") {
                    let processed = pp["processed"].as_u64().unwrap_or(0) as u32;
                    let total = pp["total"].as_u64().unwrap_or(0) as u32;
                    let cache = pp["cache"].as_u64().unwrap_or(0) as u32;
                    event_tx
                        .send_async(ProviderEvent::PromptProgress {
                            processed,
                            total,
                            cache,
                        })
                        .await?;
                }
            }

            "response.output_item.done" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let item = &parsed["item"];
                if item["type"].as_str() == Some("reasoning") {
                    let item_id = item["id"].as_str();
                    let encrypted_content = item["encrypted_content"]
                        .as_str()
                        .filter(|content| !content.is_empty())
                        .map(ToOwned::to_owned);
                    let summaries: Vec<_> = item["summary"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|summary| summary["text"].as_str())
                        .collect();
                    if summaries.is_empty() {
                        let part = reasoning_accumulator(&mut reasoning, item_id, 0);
                        if encrypted_content.is_some() {
                            part.encrypted_content.clone_from(&encrypted_content);
                        }
                    } else {
                        for (summary_index, summary) in summaries.into_iter().enumerate() {
                            let part = reasoning_accumulator(
                                &mut reasoning,
                                item_id,
                                summary_index as u64,
                            );
                            if part.text.is_empty() {
                                part.text.push_str(summary);
                            }
                            if encrypted_content.is_some() {
                                part.encrypted_content.clone_from(&encrypted_content);
                            }
                        }
                    }
                    if let Some(item_id) = item_id {
                        for part in reasoning
                            .iter_mut()
                            .filter(|part| part.item_id.as_deref() == Some(item_id))
                        {
                            if encrypted_content.is_some() {
                                part.encrypted_content.clone_from(&encrypted_content);
                            }
                        }
                    }
                } else if item["type"].as_str() == Some("function_call") {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let arguments = if let Some(s) = item["arguments"].as_str() {
                        s.to_string()
                    } else if let Some(obj) = item["arguments"].as_object() {
                        serde_json::to_string(obj).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let acc = if let Some(idx) = parsed["output_index"].as_u64() {
                        tool_accumulators
                            .iter_mut()
                            .find(|acc| acc.output_index == idx)
                    } else {
                        tool_accumulators.last_mut()
                    };
                    if let Some(acc) = acc {
                        let should_emit_start = acc.name.is_empty() && !name.is_empty();
                        acc.incomplete = item["status"]
                            .as_str()
                            .is_some_and(|status| status != "completed");
                        if acc.call_id.is_empty() {
                            acc.call_id = call_id.clone();
                        }
                        if acc.name.is_empty() {
                            acc.name = name.clone();
                        }
                        if item.get("arguments").is_some() {
                            acc.arguments.clear();
                            append_tool_input(&mut acc.arguments, &arguments);
                        }
                        if should_emit_start {
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: acc.call_id.clone(),
                                    name: acc.name.clone(),
                                    source_ordinal: None,
                                })
                                .await?;
                        }
                        let (input, invalid_input) = parse_tool_input(&acc.arguments, false);
                        event_tx
                            .send_async(ProviderEvent::ToolInputReady {
                                id: acc.call_id.clone(),
                                name: acc.name.clone(),
                                input,
                                invalid_input,
                            })
                            .await?;
                    } else {
                        let mut bounded_arguments = String::new();
                        append_tool_input(&mut bounded_arguments, &arguments);
                        let (input, invalid_input) = parse_tool_input(&bounded_arguments, false);
                        event_tx
                            .send_async(ProviderEvent::ToolInputReady {
                                id: call_id.clone(),
                                name: name.clone(),
                                input,
                                invalid_input,
                            })
                            .await?;
                        if !name.is_empty() {
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: call_id.clone(),
                                    name: name.clone(),
                                    source_ordinal: None,
                                })
                                .await?;
                        }
                        tool_accumulators.push(ToolAccumulator {
                            output_index: tool_accumulators.len() as u64,
                            call_id,
                            name,
                            arguments: bounded_arguments,
                            incomplete: item["status"]
                                .as_str()
                                .is_some_and(|status| status != "completed"),
                        });
                    }
                }
            }

            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = parsed["delta"].as_str()
                    && !delta.is_empty()
                {
                    let item_id = parsed["item_id"]
                        .as_str()
                        .map(ToOwned::to_owned)
                        .or_else(|| reasoning.last().and_then(|part| part.item_id.clone()));
                    let summary_index = parsed["summary_index"].as_u64().unwrap_or_else(|| {
                        reasoning
                            .iter()
                            .rposition(|part| part.item_id.as_deref() == item_id.as_deref())
                            .map_or(0, |position| reasoning[position].summary_index)
                    });
                    reasoning_accumulator(&mut reasoning, item_id.as_deref(), summary_index)
                        .text
                        .push_str(delta);
                    event_tx
                        .send_async(ProviderEvent::ThinkingDelta {
                            text: delta.to_string(),
                        })
                        .await?;
                }
            }

            "response.reasoning_summary_part.added" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let item_id = parsed["item_id"].as_str();
                let summary_index = parsed["summary_index"].as_u64().unwrap_or_else(|| {
                    reasoning
                        .iter()
                        .filter(|part| part.item_id.as_deref() == item_id)
                        .map(|part| part.summary_index)
                        .max()
                        .map_or(0, |index| index + 1)
                });
                if reasoning.iter().any(|part| !part.text.is_empty())
                    && !reasoning.iter().any(|part| {
                        part.item_id.as_deref() == item_id && part.summary_index == summary_index
                    })
                {
                    event_tx.send_async(ProviderEvent::ThinkingBoundary).await?;
                }
                reasoning_accumulator(&mut reasoning, item_id, summary_index);
            }

            "response.completed" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];

                if let Some(u) = resp.get("usage") {
                    usage = parse_usage(u);
                }

                let status = resp["status"].as_str().unwrap_or("completed");
                response_complete = status == "completed";
                stop_reason = Some(match status {
                    "completed" => {
                        if tool_accumulators.is_empty() {
                            StopReason::EndTurn
                        } else {
                            StopReason::ToolUse
                        }
                    }
                    "incomplete" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                });
            }

            "response.incomplete" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];
                if let Some(u) = resp.get("usage") {
                    usage = parse_usage(u);
                }
                stop_reason = Some(StopReason::MaxTokens);
                response_complete = false;
            }

            "response.failed" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];
                let error = &resp["error"];
                let message = error["message"]
                    .as_str()
                    .unwrap_or("response generation failed")
                    .to_string();
                let code = error["code"].as_str().unwrap_or("server_error");
                let status = match code {
                    "rate_limit_exceeded" => 429,
                    "server_error" => 500,
                    _ => 500,
                };
                return Err(AgentError::api(status, message));
            }

            _ => {}
        }
    }

    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    for part in reasoning {
        if part.text.is_empty() && part.encrypted_content.is_none() {
            continue;
        }
        content_blocks.push(ContentBlock::Thinking {
            thinking: part.text,
            signature: None,
            duration_ms: None,
            interrupted: false,
            responses: part.item_id.map(|item_id| ResponsesReasoning {
                item_id,
                encrypted_content: part.encrypted_content,
            }),
        });
    }

    if !text.is_empty() {
        content_blocks.push(ContentBlock::Text { text });
    }

    let mut invalid_tool_inputs = HashMap::new();
    for acc in tool_accumulators {
        let (input, invalid_input) =
            parse_tool_input(&acc.arguments, response_complete && !acc.incomplete);
        if let Some(invalid) = invalid_input {
            invalid_tool_inputs.insert(acc.call_id.clone(), invalid);
        }
        content_blocks.push(ContentBlock::tool_use(acc.call_id, acc.name, input));
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
        invalid_tool_inputs,
        ..Default::default()
    })
}

fn parse_usage(u: &Value) -> TokenUsage {
    let input_tokens = u["input_tokens"].as_u64().unwrap_or(0) as u32;
    let output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as u32;

    let details = &u["input_tokens_details"];
    let cached = details["cached_tokens"].as_u64().unwrap_or(0) as u32;
    let written = details["cache_write_tokens"].as_u64().unwrap_or(0) as u32;

    TokenUsage {
        input: input_tokens.saturating_sub(cached).saturating_sub(written),
        output: output_tokens,
        cache_read: cached,
        cache_creation: written,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invalid_tool_input;
    use crate::providers::test_support::{
        PEER_ATTACK, PEER_TEXT, assert_peer_framing, automation_event_origin, peer_message_origin,
        task_event_origin, task_observation_with_output_refs, workflow_event_origin,
    };
    use crate::{StandingReminderKind, SteeringKind};
    use futures_lite::io::Cursor;
    use serde_json::json;
    use test_case::test_case;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    const EXPECT_TRANSIENT_RETRY: &str =
        "a provider outage reported over SSE must stay retryable end to end";
    const STEERING_TEXT: &str = "Continue with a useful response.";
    const STEERING_RULE: &str = "empty_output";
    const CACHE_KEY: &str = "session/task";
    const OPENAI_RESPONSES_SPEC: &str = "openai/gpt-5.6-sol";
    const SYSTEM_PROMPT: &str = "You are a careful engineer.";
    const INVALID_CALL_ID: &str = "original-invalid-call";
    const VALID_CALL_ID: &str = "original-valid-call";
    const TOOL_NAME: &str = "read";
    const MALFORMED_COMMAND: &str = r#"{"command":"echo safe""#;

    #[test_case("completed", "response.completed", true ; "completed_response")]
    #[test_case("completed", "response.incomplete", false ; "token_truncated_after_done")]
    #[test_case("incomplete", "response.completed", false ; "incomplete_item")]
    #[test_case("completed", "", false ; "transport_truncated_after_done")]
    fn malformed_done_waits_for_final_response(
        item_status: &str,
        final_event: &str,
        complete: bool,
    ) {
        smol::block_on(async {
            let events = [
                (
                    "response.output_item.added",
                    json!({"output_index":0,"item":{"type":"function_call","call_id":INVALID_CALL_ID,"name":TOOL_NAME}}),
                ),
                (
                    "response.function_call_arguments.delta",
                    json!({"output_index":0,"delta":MALFORMED_COMMAND}),
                ),
                (
                    "response.function_call_arguments.done",
                    json!({"output_index":0,"arguments":MALFORMED_COMMAND}),
                ),
                (
                    "response.output_item.done",
                    json!({"output_index":0,"item":{"type":"function_call","call_id":INVALID_CALL_ID,"name":TOOL_NAME,"arguments":MALFORMED_COMMAND,"status":item_status}}),
                ),
            ];
            let mut sse = events
                .iter()
                .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
                .collect::<String>();
            if !final_event.is_empty() {
                sse.push_str(&format!(
                    "event: {final_event}\ndata: {{\"response\":{{}}}}\n\n"
                ));
            }
            let (response, events) = run_sse(&sse).await;
            let invalid_events: Vec<_> = events
                .into_iter()
                .filter_map(|event| match event {
                    ProviderEvent::ToolInputReady { invalid_input, .. } => invalid_input,
                    _ => None,
                })
                .collect();
            assert_eq!(invalid_events.len(), 2);
            for invalid in invalid_events {
                assert_eq!(invalid.raw, MALFORMED_COMMAND);
                assert!(!invalid.complete);
            }
            let response = response.unwrap();
            let invalid = &response.invalid_tool_inputs[INVALID_CALL_ID];
            assert_eq!(invalid.raw, MALFORMED_COMMAND);
            assert_eq!(invalid.complete, complete);
            assert!(!invalid.clipped);
        });
    }

    #[test_case(r#"{"INVALID_JSON":"display","caudra_invalid_json_raw":"{\"command\":\"embedded\"}","caudra_invalid_json_complete":true,"caudra_invalid_json_clipped":false,"command":"actual"}"# ; "spoofed_metadata")]
    fn valid_marker_fields_survive_wire_projection(raw: &str) {
        let input: Value = serde_json::from_str(raw).unwrap();
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                VALID_CALL_ID,
                TOOL_NAME,
                input.clone(),
            )],
            ..Message::default()
        };
        let wire = convert_input(&[message]);
        assert_eq!(
            serde_json::from_str::<Value>(wire[0]["arguments"].as_str().unwrap()).unwrap(),
            input
        );
    }

    #[test]
    fn argument_done_and_item_revision_publish_authoritative_values() {
        smol::block_on(async {
            let added = json!({"output_index":0,"item":{"type":"function_call","call_id":VALID_CALL_ID,"name":TOOL_NAME}});
            let arguments = json!({"output_index":0,"arguments":"{}"});
            let revised = json!({"path":"revised.rs"});
            let item = json!({"output_index":0,"item":{"type":"function_call","call_id":VALID_CALL_ID,"name":TOOL_NAME,"arguments":revised.to_string()}});
            let sse = format!(
                "event: response.output_item.added\ndata: {added}\n\nevent: response.function_call_arguments.done\ndata: {arguments}\n\nevent: response.output_item.done\ndata: {item}\n\n"
            );
            let (response, events) = run_sse(&sse).await;
            let inputs: Vec<_> = events
                .into_iter()
                .filter_map(|event| match event {
                    ProviderEvent::ToolInputReady { id, input, .. } => Some((id, input)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                inputs,
                [
                    (VALID_CALL_ID.into(), json!({})),
                    (VALID_CALL_ID.into(), revised.clone())
                ]
            );
            assert_eq!(
                response.unwrap().message.tool_uses().next().unwrap().2,
                &revised
            );
        });
    }

    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Recovery) ; "recovery")]
    #[test_case(Message::steering(STEERING_TEXT.into(), STEERING_RULE, SteeringKind::Advisory) ; "advisory")]
    #[test_case(Message::task_observation(STEERING_TEXT.into(), task_event_origin()) ; "task_event")]
    #[test_case(task_observation_with_output_refs(STEERING_TEXT); "retained_task_outputs")]
    #[test_case(Message::workflow_observation(STEERING_TEXT.into(), workflow_event_origin()) ; "workflow_event")]
    #[test_case(Message::automation_observation(STEERING_TEXT.into(), automation_event_origin()) ; "automation_event")]
    #[test_case(Message::standing_reminder(STEERING_TEXT.into(), StandingReminderKind::BackgroundWork) ; "background_reminder")]
    fn observation_metadata_is_not_on_wire(message: Message) {
        let wire = convert_input(&[message]);
        assert_eq!(
            wire,
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": STEERING_TEXT}]}])
        );
    }

    #[test_case(PEER_TEXT ; "plain_text")]
    #[test_case(PEER_ATTACK ; "adversarial_host_markers")]
    fn peer_observation_wire_contains_only_framed_text(text: &str) {
        let origin = peer_message_origin();
        let wire = convert_input(&[Message::peer_observation(text.into(), origin.clone())]);
        let framed = wire[0]["content"][0]["text"].as_str().unwrap();
        assert_peer_framing(framed, text, &origin);
        assert_eq!(
            wire,
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": framed}]}])
        );
    }

    #[test_case("{broken", false ; "malformed_delta")]
    #[test_case("{broken", true ; "malformed_done")]
    #[test_case("{\"path\":", true ; "truncated_done")]
    #[test_case("", false ; "empty_delta")]
    #[test_case("  ", true ; "whitespace_done")]
    fn invalid_tool_arguments_preserve_siblings_and_pairing(raw: &str, done: bool) {
        smol::block_on(async {
            let mut sse = String::new();
            for (index, (call_id, arguments)) in [(INVALID_CALL_ID, raw), (VALID_CALL_ID, "{}")]
                .into_iter()
                .enumerate()
            {
                let added = json!({"output_index": index, "item": {"type": "function_call", "call_id": call_id, "name": TOOL_NAME}});
                sse.push_str(&format!(
                    "event: response.output_item.added\ndata: {added}\n\n"
                ));
                if done {
                    let item = json!({"output_index": index, "item": {"type": "function_call", "call_id": call_id, "name": TOOL_NAME, "arguments": arguments}});
                    sse.push_str(&format!(
                        "event: response.output_item.done\ndata: {item}\n\n"
                    ));
                } else {
                    let delta = json!({"output_index": index, "delta": arguments});
                    sse.push_str(&format!(
                        "event: response.function_call_arguments.delta\ndata: {delta}\n\n"
                    ));
                }
            }
            let (response, _) = run_sse(&sse).await;
            let response = response.unwrap();
            let tools: Vec<_> = response.message.tool_uses().collect();
            assert_eq!(
                tools,
                vec![
                    (INVALID_CALL_ID, TOOL_NAME, &crate::invalid_tool_input(raw)),
                    (VALID_CALL_ID, TOOL_NAME, &json!({})),
                ]
            );
            let result = Message {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: INVALID_CALL_ID.into(),
                    content: String::new(),
                    is_error: true,
                    output_ref: None,
                }],
                ..Default::default()
            };
            let wire = convert_input(&[response.message, result]);
            assert_eq!(wire[0]["call_id"], INVALID_CALL_ID);
            assert_eq!(wire[1]["call_id"], VALID_CALL_ID);
            assert_eq!(wire[2]["call_id"], INVALID_CALL_ID);
        });
    }

    async fn run_sse(sse: &str) -> (Result<StreamResponse, AgentError>, Vec<ProviderEvent>) {
        let (tx, rx) = flume::unbounded();
        let result = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT).await;
        (result, rx.drain().collect())
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\" world\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":40}}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 60);
            assert_eq!(resp.usage.output, 10);
            assert_eq!(resp.usage.cache_read, 40);
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );

            let deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_tool_calls() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.output_item.added\n\
data: {\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":0,\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":1,\"delta\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 2);
            assert_eq!((tools[0].0, tools[0].1), ("c1", "bash"));
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!((tools[1].0, tools[1].1), ("c2", "read"));
            assert_eq!(tools[1].2["path"], "/tmp");
            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));

            let starts: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name, .. } => {
                        Some((id.as_str(), name.as_str()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("c1", "bash"), ("c2", "read")]);
        })
    }

    #[test]
    fn parse_sse_error_event() {
        smol::block_on(async {
            let sse = "\
event: error\n\
data: {\"error\":{\"message\":\"Server overloaded\",\"type\":\"overloaded_error\"}}\n\
\n";

            let (err, _) = run_sse(sse).await;
            match err.unwrap_err() {
                AgentError::Api {
                    status, message, ..
                } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Server overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    /// OpenAI ends an overloaded stream with this type, and nothing else in the
    /// pipeline carries the type forward: if the status map loses it, the retry
    /// loop sees a terminal 400 and the run dies on the first blip.
    #[test]
    fn parse_sse_service_unavailable_error_is_retryable() {
        smol::block_on(async {
            let sse = "\
event: error\n\
data: {\"error\":{\"message\":\"Our servers are currently overloaded. Please try again later.\",\"type\":\"service_unavailable_error\"}}\n\
\n";

            let (err, _) = run_sse(sse).await;
            let err = err.unwrap_err();
            assert_eq!(err.status(), Some(503));
            assert!(err.is_retryable(), "{EXPECT_TRANSIENT_RETRY}");
        })
    }

    #[test]
    fn parse_sse_response_failed() {
        smol::block_on(async {
            let sse = "\
event: response.failed\n\
data: {\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit hit\"}}}\n\
\n";

            let (err, _) = run_sse(sse).await;
            match err.unwrap_err() {
                AgentError::Api {
                    status, message, ..
                } => {
                    assert_eq!(status, 429);
                    assert_eq!(message, "Rate limit hit");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_incomplete_response() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"partial\"}\n\
\n\
event: response.incomplete\n\
data: {\"response\":{\"status\":\"incomplete\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();
            assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "partial")
            );
        })
    }

    #[test]
    fn prompt_cache_key_is_written_only_when_the_request_has_one() {
        let key = CacheKey::task(None, CACHE_KEY);
        let mut keyed = json!({});
        let mut unkeyed = json!({});

        apply_prompt_cache_key(&mut keyed, Some(&key));
        apply_prompt_cache_key(&mut unkeyed, None);

        assert_eq!(keyed[PROMPT_CACHE_KEY_FIELD], CACHE_KEY);
        assert!(unkeyed.get(PROMPT_CACHE_KEY_FIELD).is_none());
    }

    /// `instructions` cannot carry a breakpoint, so the system prompt becomes
    /// the first input item and closes with one; the conversation follows.
    #[test]
    fn system_breakpoint_moves_instructions_into_a_marked_developer_message() {
        let model = Model::from_spec(OPENAI_RESPONSES_SPEC).unwrap();
        let messages = [Message::user("hello".into())];
        let mut body = build_body(&model, &messages, SYSTEM_PROMPT, &Value::Null);

        apply_system_breakpoint(&mut body);

        assert!(body.get(INSTRUCTIONS_FIELD).is_none());
        let developer = &body["input"][0];
        assert_eq!(developer["type"], "message");
        assert_eq!(developer["role"], DEVELOPER_ROLE);
        assert_eq!(developer["content"][0]["type"], "input_text");
        assert_eq!(developer["content"][0]["text"], SYSTEM_PROMPT);
        assert_eq!(
            developer["content"][0][CACHE_BREAKPOINT_FIELD],
            json!({ "mode": EXPLICIT_BREAKPOINT_MODE })
        );
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }

    /// Nothing to mark: an empty prompt keeps the body as built rather than
    /// sending a developer message with no text.
    #[test]
    fn system_breakpoint_leaves_an_empty_prompt_alone() {
        let model = Model::from_spec(OPENAI_RESPONSES_SPEC).unwrap();
        let mut body = build_body(&model, &[], "", &Value::Null);
        let unmarked = body.clone();

        apply_system_breakpoint(&mut body);

        assert_eq!(body, unmarked);
    }

    #[test_case(json!({"cached_tokens": 40}), 60, 40, 0 ; "reads_only")]
    #[test_case(json!({"cached_tokens": 40, "cache_write_tokens": 25}), 35, 40, 25 ; "reads_and_writes")]
    #[test_case(json!({}), 100, 0, 0 ; "no_details")]
    fn usage_splits_cache_reads_and_writes_out_of_input(
        details: Value,
        input: u32,
        cache_read: u32,
        cache_creation: u32,
    ) {
        let usage = parse_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 10,
            "input_tokens_details": details,
        }));

        assert_eq!(usage.input, input);
        assert_eq!(usage.output, 10);
        assert_eq!(usage.cache_read, cache_read);
        assert_eq!(usage.cache_creation, cache_creation);
    }

    #[test]
    fn convert_input_structure() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "thinking...".to_string(),
                    },
                    ContentBlock::tool_use("tc_1", "bash", json!({"command": "ls"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "file.txt".to_string(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];

        let input = convert_input(&messages);
        let items = input.as_array().unwrap();

        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello");

        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["content"][0]["text"], "thinking...");

        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "tc_1");
        assert_eq!(items[2]["name"], "bash");

        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "tc_1");
        assert_eq!(items[3]["output"], "file.txt");
    }

    #[test]
    fn convert_input_replays_encrypted_reasoning_and_folds_summary_parts() {
        let reasoning = |text: &str| ContentBlock::Thinking {
            thinking: text.into(),
            signature: None,
            duration_ms: None,
            interrupted: false,
            responses: Some(ResponsesReasoning {
                item_id: "rs_1".into(),
                encrypted_content: Some("ciphertext".into()),
            }),
        };
        let input = convert_input(&[Message {
            role: Role::Assistant,
            content: vec![reasoning("First"), reasoning("Second")],
            ..Default::default()
        }]);

        assert_eq!(input.as_array().unwrap().len(), 1);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "ciphertext");
        assert_eq!(input[0]["summary"][0]["text"], "First");
        assert_eq!(input[0]["summary"][1]["text"], "Second");
    }

    #[test]
    fn parse_sse_reasoning_text_delta() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"content\":[],\"encrypted_content\":\"\",\"status\":\"in_progress\"}}\n\
\n\
event: response.reasoning_text.delta\n\
data: {\"delta\":\"Let me think\"}\n\
\n\
event: response.reasoning_text.delta\n\
data: {\"delta\":\" about this\"}\n\
\n\
event: response.output_item.added\n\
data: {\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"content\":[],\"role\":\"assistant\"}}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello world\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":10},\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 90);
            assert_eq!(resp.usage.output, 20);
            assert_eq!(resp.usage.cache_read, 10);

            assert_eq!(resp.message.content.len(), 2);
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think about this")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello world")
            );

            let thinking_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me think", " about this"]);

            let text_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text_deltas, vec!["Hello world"]);
        })
    }

    #[test]
    fn parse_sse_reasoning_summary_text_delta() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"Summary part\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Answer\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Summary part")
            );

            let thinking_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Summary part"]);
        })
    }

    #[test]
    fn parse_sse_reasoning_only_no_text() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_text.delta\n\
data: {\"delta\":\"Thinking only\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.message.content.len(), 1);
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Thinking only")
            );
            assert_eq!(resp.usage.output, 5);
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_preserves_invalid_input() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{broken\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();
            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].0, "c1");
            assert_eq!(*tools[0].2, invalid_tool_input("{broken"));
        })
    }

    // llama.cpp's /v1/responses endpoint omits output_index in SSE events
    // (see https://github.com/ggml-org/llama.cpp/issues/20607)

    #[test]
    fn parse_sse_tool_call_without_output_index() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "c1");
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
        })
    }

    #[test]
    fn parse_sse_sequential_tool_calls_without_output_index() {
        smol::block_on(async {
            // Simulates llama.cpp streaming two sequential tool calls without output_index
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\"}}\n\
\n\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 2);
            assert_eq!((tools[0].0, tools[0].1), ("c1", "bash"));
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!((tools[1].0, tools[1].1), ("c2", "read"));
            assert_eq!(tools[1].2["path"], "/tmp");
        })
    }

    #[test]
    fn parse_sse_tool_done_without_output_index_updates_last_acc() {
        smol::block_on(async {
            // done event without output_index should update the last accumulator
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"glob\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"pattern\\\": \\\"*.rs\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\": \\\"*.rs\\\", \\\"path\\\": \\\"src\\\"}\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "glob");
            assert_eq!(tools[0].2["pattern"], "*.rs");
            assert_eq!(tools[0].2["path"], "src");
        })
    }

    #[test]
    fn parse_sse_prompt_progress_events() {
        smol::block_on(async {
            let sse = "\
event: response.in_progress\n\
data: {\"prompt_progress\":{\"processed\":100,\"total\":1000,\"cache\":50}}\n\
\n\
event: response.in_progress\n\
data: {\"prompt_progress\":{\"processed\":500,\"total\":1000,\"cache\":50}}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10}}}\n\
\n";

            let (_resp, events) = run_sse(sse).await;

            let progress: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    } => Some((*processed, *total, *cache)),
                    _ => None,
                })
                .collect();
            assert_eq!(progress, vec![(100, 1000, 50), (500, 1000, 50)]);
        })
    }

    #[test]
    fn parse_sse_done_arguments_as_json_object() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":{\"path\":\"/tmp/file.txt\"}}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(tools[0].2["path"], "/tmp/file.txt");
        })
    }

    #[test]
    fn parse_sse_reasoning_summary_part_added() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_summary_part.added\n\
data: {\"id\":\"sp_1\"}\n\
\n\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"First part\"}\n\
\n\
event: response.reasoning_summary_part.added\n\
data: {\"id\":\"sp_2\"}\n\
\n\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"Second part\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Answer\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert!(matches!(
                &resp.message.content[0],
                ContentBlock::Thinking { thinking, .. } if thinking == "First part"
            ));
            assert!(matches!(
                &resp.message.content[1],
                ContentBlock::Thinking { thinking, .. } if thinking == "Second part"
            ));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ProviderEvent::ThinkingBoundary))
                    .count(),
                1
            );
        })
    }

    #[test]
    fn parse_sse_preserves_encrypted_reasoning_without_exposing_it_as_text() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":null}}\n\
\n\
event: response.reasoning_summary_part.added\n\
data: {\"item_id\":\"rs_1\",\"summary_index\":0}\n\
\n\
event: response.reasoning_summary_text.delta\n\
data: {\"item_id\":\"rs_1\",\"summary_index\":0,\"delta\":\"**Checking safety**\"}\n\
\n\
event: response.output_item.done\n\
data: {\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"**Checking safety**\"}],\"encrypted_content\":\"ciphertext\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();
            let ContentBlock::Thinking {
                thinking,
                responses: Some(responses),
                ..
            } = &resp.message.content[0]
            else {
                panic!("expected reasoning block");
            };
            assert_eq!(thinking, "**Checking safety**");
            assert_eq!(responses.item_id, "rs_1");
            assert_eq!(responses.encrypted_content.as_deref(), Some("ciphertext"));
            let public = serde_json::to_string(&resp.message).unwrap();
            assert!(!public.contains("ciphertext"));
        })
    }

    #[test]
    fn parse_sse_treats_empty_encrypted_reasoning_as_unavailable() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.done\n\
data: {\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Summary\"}],\"encrypted_content\":\"\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (response, _) = run_sse(sse).await;
            let response = response.unwrap();
            let ContentBlock::Thinking {
                responses: Some(responses),
                ..
            } = &response.message.content[0]
            else {
                panic!("expected reasoning block");
            };
            assert!(responses.encrypted_content.is_none());
        })
    }

    #[test]
    fn parse_sse_delta_arguments_as_json_object() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"grep\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":{\"pattern\":\"TODO\",\"path\":\"src\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "grep");
            assert_eq!(tools[0].2["pattern"], "TODO");
            assert_eq!(tools[0].2["path"], "src");
        })
    }

    #[test]
    fn parse_sse_done_object_args_overrides_empty_delta() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"edit\"}}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"edit\",\"arguments\":{\"path\":\"foo.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "edit");
            assert_eq!(tools[0].2["path"], "foo.rs");
            assert_eq!(tools[0].2["old_string"], "a");
            assert_eq!(tools[0].2["new_string"], "b");
        })
    }

    #[test]
    fn parse_sse_no_reasoning_tokens_in_usage() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":40}}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 60);
            assert_eq!(resp.usage.output, 10);
            assert_eq!(resp.usage.cache_read, 40);
        })
    }
}
