use std::collections::{HashMap, HashSet};

pub use caudra_storage::id::CaudraId;
use caudra_storage::sessions::TitleSource;
use caudra_storage::tool_outputs::ToolOutputRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::types::{ContentBlock, EMPTY_RESPONSE_MARKER, ImageSource, Message, MessageKind, Role};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: CaudraId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<CaudraId>,
    pub group_id: CaudraId,
    #[serde(flatten)]
    pub kind: HistoryItemKind,
}

impl TitleSource for HistoryItem {
    fn first_user_text(&self) -> Option<&str> {
        let HistoryItemKind::User {
            text,
            display_text,
            origin: UserOrigin::Turn,
            ..
        } = &self.kind
        else {
            return None;
        };
        match display_text {
            Some(display) if !display.is_empty() => Some(display),
            Some(_) => None,
            None if !text.trim().is_empty() => Some(text),
            None => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryItemKind {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageSource>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
        origin: UserOrigin,
    },
    AssistantText {
        text: String,
        state: AssistantTextState,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retained_output_refs: Vec<ToolOutputRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retained_subagent_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_compaction_summary: bool,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        redacted: bool,
        interrupted: bool,
        /// Dropped by `project_group`, so it never reaches a provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<ToolOutputRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageSource>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserOrigin {
    #[default]
    Turn,
    Observation,
    Synthetic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantTextState {
    #[default]
    Complete,
    Interrupted,
    Padding,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HistoryProjectionError {
    #[error("duplicate history item id {id}")]
    DuplicateItemId { id: CaudraId },
    #[error(
        "history item {item_id} has parent {actual_parent_id:?}, expected {expected_parent_id}"
    )]
    InvalidParentLink {
        item_id: CaudraId,
        expected_parent_id: CaudraId,
        actual_parent_id: Option<CaudraId>,
    },
    #[error("history group {group_id} is not contiguous")]
    NoncontiguousGroup { group_id: CaudraId },
    #[error("history group {group_id} mixes user and assistant items")]
    MixedGroupKinds { group_id: CaudraId },
    #[error("tool result {item_id} references orphan call {call_id:?}")]
    OrphanToolResult { item_id: CaudraId, call_id: String },
    #[error("tool call {item_id} duplicates call ID {call_id:?} in its parallel batch")]
    DuplicateToolCall { item_id: CaudraId, call_id: String },
    #[error("tool-call group {call_group_id} is missing an immediate result for call {call_id:?}")]
    MissingToolResult {
        call_group_id: CaudraId,
        call_id: String,
    },
    #[error("history head {head_id} does not exist")]
    MissingHead { head_id: CaudraId },
    #[error("history item {item_id} references missing parent {parent_id}")]
    MissingParent {
        item_id: CaudraId,
        parent_id: CaudraId,
    },
    #[error("history contains a parent cycle at item {item_id}")]
    ParentCycle { item_id: CaudraId },
}

impl HistoryProjectionError {
    pub fn is_sanitizable_tool_order_error(&self) -> bool {
        matches!(
            self,
            Self::OrphanToolResult { .. } | Self::MissingToolResult { .. }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    User,
    Assistant,
}

impl HistoryItemKind {
    fn group_kind(&self) -> GroupKind {
        match self {
            Self::User { .. } | Self::ToolResult { .. } => GroupKind::User,
            Self::AssistantText { .. } | Self::Reasoning { .. } | Self::ToolCall { .. } => {
                GroupKind::Assistant
            }
        }
    }
}

/// Expands one provider-facing message into parent-linked atomic items sharing
/// a newly generated group id.
pub fn expand_message(message: &Message, parent_id: Option<CaudraId>) -> Vec<HistoryItem> {
    let group_id = CaudraId::generate();
    let kinds = match message.role {
        Role::User => expand_user_message(message),
        Role::Assistant => expand_assistant_message(message),
    };
    let mut parent_id = parent_id;

    kinds
        .into_iter()
        .map(|kind| {
            let id = CaudraId::generate();
            let item = HistoryItem {
                id,
                parent_id,
                group_id,
                kind,
            };
            parent_id = Some(id);
            item
        })
        .collect()
}

/// Validates and projects an ancestor-ordered item path into provider-facing
/// grouped messages.
pub fn project_messages(items: &[HistoryItem]) -> Result<Vec<Message>, HistoryProjectionError> {
    validate_items(items)?;

    let mut messages = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let group_id = items[start].group_id;
        let end = items[start..]
            .iter()
            .position(|item| item.group_id != group_id)
            .map_or(items.len(), |offset| start + offset);
        messages.push(project_group(&items[start..end]));
        start = end;
    }
    Ok(messages)
}

/// Resolves the persisted head. Sessions written before heads were introduced
/// are linear, so their last item remains active. A staged revert may
/// intentionally select the empty root and must not take that fallback.
pub fn resolve_history_head(
    items: &[HistoryItem],
    stored_head: Option<CaudraId>,
    has_pending_revert: bool,
) -> Option<CaudraId> {
    if stored_head.is_some() || has_pending_revert {
        stored_head
    } else {
        items.last().map(|item| item.id)
    }
}

/// Returns the root-to-head path selected from an all-node history graph.
pub fn active_history_items(
    items: &[HistoryItem],
    head: Option<CaudraId>,
) -> Result<Vec<HistoryItem>, HistoryProjectionError> {
    let index = validate_history_graph(items)?;
    let Some(mut current) = head else {
        return Ok(Vec::new());
    };
    if !index.contains_key(&current) {
        return Err(HistoryProjectionError::MissingHead { head_id: current });
    }

    let mut path = Vec::new();
    while let Some(&position) = index.get(&current) {
        let item = &items[position];
        path.push(item.clone());
        let Some(parent_id) = item.parent_id else {
            break;
        };
        current = parent_id;
    }
    path.reverse();
    validate_item_structure(&path)?;
    Ok(path)
}

/// Merges an active runtime path into the persisted all-node graph by item ID.
/// Existing IDs adopt their latest representation and unseen IDs are appended;
/// nodes absent from the active path are never removed.
pub fn merge_history_items(
    stored: &mut Vec<HistoryItem>,
    active: &[HistoryItem],
) -> Result<(), HistoryProjectionError> {
    validate_items(active)?;
    let mut merged = stored.clone();
    let mut index = validate_history_graph(&merged)?;
    for item in active {
        if let Some(&position) = index.get(&item.id) {
            merged[position] = item.clone();
        } else {
            index.insert(item.id, merged.len());
            merged.push(item.clone());
        }
    }
    validate_history_graph(&merged)?;
    *stored = merged;
    Ok(())
}

fn validate_history_graph(
    items: &[HistoryItem],
) -> Result<HashMap<CaudraId, usize>, HistoryProjectionError> {
    let mut index = HashMap::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        if index.insert(item.id, position).is_some() {
            return Err(HistoryProjectionError::DuplicateItemId { id: item.id });
        }
    }
    for item in items {
        if let Some(parent_id) = item.parent_id
            && !index.contains_key(&parent_id)
        {
            return Err(HistoryProjectionError::MissingParent {
                item_id: item.id,
                parent_id,
            });
        }
    }

    let mut complete = HashSet::new();
    for item in items {
        let mut current = item.id;
        let mut visiting = HashSet::new();
        while !complete.contains(&current) {
            if !visiting.insert(current) {
                return Err(HistoryProjectionError::ParentCycle { item_id: current });
            }
            let node = &items[index[&current]];
            let Some(parent_id) = node.parent_id else {
                break;
            };
            current = parent_id;
        }
        complete.extend(visiting);
    }
    Ok(index)
}

fn expand_user_message(message: &Message) -> Vec<HistoryItemKind> {
    let origin = if message.kind == MessageKind::Observation {
        UserOrigin::Observation
    } else if message.display_text.as_deref() == Some("") {
        UserOrigin::Synthetic
    } else {
        UserOrigin::Turn
    };
    let has_tool_results = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    if !has_tool_results {
        return expand_user_content(message, origin);
    }

    let mut kinds = Vec::new();
    if origin != UserOrigin::Turn || message.display_text.is_some() {
        kinds.push(user_kind(String::new(), Vec::new(), message, origin));
    }
    let result_count = message
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .count();
    let trailing_images = message
        .content
        .iter()
        .rev()
        .take_while(|block| matches!(block, ContentBlock::Image { .. }))
        .count();
    let has_image_owners = message.tool_result_image_owners.len() == trailing_images;
    // Legacy v2 messages had no image-owner metadata. Complete image batches
    // recover positionally; ambiguous sparse batches fall back to the last
    // result, matching the old best effort.
    let pair_trailing_images = trailing_images == result_count;
    let mut result_indexes = Vec::with_capacity(result_count);
    let mut trailing_image_index = 0;
    let mut pending_images = Vec::new();
    let mut last_tool_result = None;
    for (block_index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Text { text } => {
                kinds.push(user_kind(
                    text.clone(),
                    std::mem::take(&mut pending_images),
                    message,
                    origin,
                ));
            }
            ContentBlock::Image { source } => {
                let is_trailing = block_index >= message.content.len() - trailing_images;
                let owner = (has_image_owners && is_trailing)
                    .then(|| message.tool_result_image_owners.get(trailing_image_index))
                    .flatten();
                let result_index = owner
                    .and_then(|owner| {
                        result_indexes
                            .iter()
                            .find_map(|(call_id, index)| (call_id == owner).then_some(*index))
                    })
                    .or_else(|| {
                        (pair_trailing_images && is_trailing)
                            .then(|| result_indexes[trailing_image_index].1)
                    })
                    .or(last_tool_result);
                if is_trailing && (has_image_owners || pair_trailing_images) {
                    trailing_image_index += 1;
                }
                if let Some(index) = result_index
                    && let HistoryItemKind::ToolResult { images, .. } = &mut kinds[index]
                {
                    images.push(source.clone());
                } else {
                    pending_images.push(source.clone());
                }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                output_ref,
            } => {
                if !pending_images.is_empty() {
                    kinds.push(user_kind(
                        String::new(),
                        std::mem::take(&mut pending_images),
                        message,
                        origin,
                    ));
                }
                kinds.push(HistoryItemKind::ToolResult {
                    call_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                    output_ref: output_ref.clone(),
                    images: Vec::new(),
                });
                last_tool_result = Some(kinds.len() - 1);
                result_indexes.push((tool_use_id.clone(), kinds.len() - 1));
            }
            other => kinds.push(assistant_kind(other, false)),
        }
    }
    if !pending_images.is_empty() {
        kinds.push(user_kind(String::new(), pending_images, message, origin));
    }
    kinds
}

fn expand_user_content(message: &Message, origin: UserOrigin) -> Vec<HistoryItemKind> {
    let mut kinds = Vec::new();
    let mut images = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Image { source } => images.push(source.clone()),
            ContentBlock::Text { text } => kinds.push(user_kind(
                text.clone(),
                std::mem::take(&mut images),
                message,
                origin,
            )),
            other => {
                if !images.is_empty() {
                    kinds.push(user_kind(
                        String::new(),
                        std::mem::take(&mut images),
                        message,
                        origin,
                    ));
                }
                kinds.push(assistant_kind(other, false));
            }
        }
    }
    if !images.is_empty() || kinds.is_empty() {
        kinds.push(user_kind(String::new(), images, message, origin));
    }
    kinds
}

fn user_kind(
    text: String,
    images: Vec<ImageSource>,
    message: &Message,
    origin: UserOrigin,
) -> HistoryItemKind {
    HistoryItemKind::User {
        text,
        images,
        display_text: message.display_text.clone(),
        origin,
    }
}

fn expand_assistant_message(message: &Message) -> Vec<HistoryItemKind> {
    let padding = message.display_text.is_none()
        && matches!(
            message.content.as_slice(),
            [ContentBlock::Text { text }] if text == EMPTY_RESPONSE_MARKER
        );
    if message.content.is_empty() {
        return vec![HistoryItemKind::AssistantText {
            text: String::new(),
            state: AssistantTextState::Complete,
            retained_output_refs: message.retained_output_refs.clone(),
            retained_subagent_ids: message.retained_subagent_ids.clone(),
            is_compaction_summary: message.is_compaction_summary,
        }];
    }
    let mut kinds: Vec<_> = message
        .content
        .iter()
        .map(|block| assistant_kind(block, padding))
        .collect();
    if let Some(HistoryItemKind::AssistantText {
        retained_output_refs,
        retained_subagent_ids,
        is_compaction_summary,
        ..
    }) = kinds
        .iter_mut()
        .find(|kind| matches!(kind, HistoryItemKind::AssistantText { .. }))
    {
        retained_output_refs.clone_from(&message.retained_output_refs);
        retained_subagent_ids.clone_from(&message.retained_subagent_ids);
        *is_compaction_summary = message.is_compaction_summary;
    }
    kinds
}

fn assistant_kind(block: &ContentBlock, padding: bool) -> HistoryItemKind {
    match block {
        ContentBlock::Text { text } => HistoryItemKind::AssistantText {
            text: text.clone(),
            state: if padding {
                AssistantTextState::Padding
            } else {
                AssistantTextState::Complete
            },
            retained_output_refs: Vec::new(),
            retained_subagent_ids: Vec::new(),
            is_compaction_summary: false,
        },
        ContentBlock::Thinking {
            thinking,
            signature,
            duration_ms,
        } => HistoryItemKind::Reasoning {
            text: thinking.clone(),
            signature: signature.clone(),
            redacted: false,
            interrupted: false,
            duration_ms: *duration_ms,
        },
        ContentBlock::RedactedThinking { data } => HistoryItemKind::Reasoning {
            text: data.clone(),
            signature: None,
            redacted: true,
            interrupted: false,
            duration_ms: None,
        },
        ContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => HistoryItemKind::ToolCall {
            call_id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            thought_signature: thought_signature.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            output_ref,
        } => HistoryItemKind::ToolResult {
            call_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
            output_ref: output_ref.clone(),
            images: Vec::new(),
        },
        ContentBlock::Image { source } => HistoryItemKind::User {
            text: String::new(),
            images: vec![source.clone()],
            display_text: None,
            origin: UserOrigin::Turn,
        },
    }
}

fn validate_items(items: &[HistoryItem]) -> Result<(), HistoryProjectionError> {
    validate_item_structure(items)?;
    validate_tool_order(items)
}

fn validate_item_structure(items: &[HistoryItem]) -> Result<(), HistoryProjectionError> {
    let mut item_ids = HashSet::with_capacity(items.len());
    let mut closed_groups = HashSet::new();
    let mut current_group = None;
    let mut current_kind = None;
    let mut previous_id = None;

    for item in items {
        if !item_ids.insert(item.id) {
            return Err(HistoryProjectionError::DuplicateItemId { id: item.id });
        }
        if let Some(expected_parent_id) = previous_id
            && item.parent_id != Some(expected_parent_id)
        {
            return Err(HistoryProjectionError::InvalidParentLink {
                item_id: item.id,
                expected_parent_id,
                actual_parent_id: item.parent_id,
            });
        }
        previous_id = Some(item.id);

        if current_group != Some(item.group_id) {
            if let Some(group_id) = current_group {
                closed_groups.insert(group_id);
            }
            if closed_groups.contains(&item.group_id) {
                return Err(HistoryProjectionError::NoncontiguousGroup {
                    group_id: item.group_id,
                });
            }
            current_group = Some(item.group_id);
            current_kind = Some(item.kind.group_kind());
        } else if current_kind != Some(item.kind.group_kind()) {
            return Err(HistoryProjectionError::MixedGroupKinds {
                group_id: item.group_id,
            });
        }
    }
    Ok(())
}

fn validate_tool_order(items: &[HistoryItem]) -> Result<(), HistoryProjectionError> {
    let mut pending_calls: Option<(CaudraId, Vec<&str>)> = None;
    let mut start = 0;

    while start < items.len() {
        let group_id = items[start].group_id;
        let end = items[start..]
            .iter()
            .position(|item| item.group_id != group_id)
            .map_or(items.len(), |offset| start + offset);
        let group = &items[start..end];
        let results: Vec<_> = group
            .iter()
            .filter_map(|item| match &item.kind {
                HistoryItemKind::ToolResult { call_id, .. } => Some((item.id, call_id.as_str())),
                _ => None,
            })
            .collect();

        if let Some((call_group_id, mut expected)) = pending_calls.take() {
            if results.is_empty() {
                return Err(HistoryProjectionError::MissingToolResult {
                    call_group_id,
                    call_id: expected[0].into(),
                });
            }
            for (item_id, call_id) in results {
                let Some(position) = expected.iter().position(|expected| *expected == call_id)
                else {
                    return Err(HistoryProjectionError::OrphanToolResult {
                        item_id,
                        call_id: call_id.into(),
                    });
                };
                expected.remove(position);
            }
            if let Some(call_id) = expected.first() {
                return Err(HistoryProjectionError::MissingToolResult {
                    call_group_id,
                    call_id: (*call_id).into(),
                });
            }
        } else if let Some((item_id, call_id)) = results.first() {
            return Err(HistoryProjectionError::OrphanToolResult {
                item_id: *item_id,
                call_id: (*call_id).into(),
            });
        }

        let calls: Vec<_> = group
            .iter()
            .filter_map(|item| match &item.kind {
                HistoryItemKind::ToolCall { call_id, .. } => Some((item.id, call_id.as_str())),
                _ => None,
            })
            .collect();
        if !calls.is_empty() {
            let mut unique = HashSet::with_capacity(calls.len());
            for (item_id, call_id) in &calls {
                if !unique.insert(*call_id) {
                    return Err(HistoryProjectionError::DuplicateToolCall {
                        item_id: *item_id,
                        call_id: (*call_id).into(),
                    });
                }
            }
            pending_calls = Some((
                group_id,
                calls.into_iter().map(|(_, call_id)| call_id).collect(),
            ));
        }
        start = end;
    }

    Ok(())
}

fn project_group(items: &[HistoryItem]) -> Message {
    let role = match items[0].kind.group_kind() {
        GroupKind::User => Role::User,
        GroupKind::Assistant => Role::Assistant,
    };
    let has_tool_results = items
        .iter()
        .any(|item| matches!(item.kind, HistoryItemKind::ToolResult { .. }));
    let mut message = Message {
        role,
        ..Default::default()
    };
    let mut has_user_metadata = false;
    let mut result_images = Vec::new();
    let mut result_image_index = 0;

    for item in items {
        match &item.kind {
            HistoryItemKind::User {
                text,
                images,
                display_text,
                origin,
            } => {
                if !has_user_metadata {
                    message.kind = match origin {
                        UserOrigin::Observation => MessageKind::Observation,
                        UserOrigin::Turn | UserOrigin::Synthetic => MessageKind::Turn,
                    };
                    message.display_text = match origin {
                        UserOrigin::Synthetic => Some(String::new()),
                        UserOrigin::Turn | UserOrigin::Observation => display_text.clone(),
                    };
                    has_user_metadata = true;
                }
                message.content.extend(
                    images
                        .iter()
                        .cloned()
                        .map(|source| ContentBlock::Image { source }),
                );
                if !text.is_empty() || (images.is_empty() && !has_tool_results) {
                    message
                        .content
                        .push(ContentBlock::Text { text: text.clone() });
                }
            }
            HistoryItemKind::AssistantText {
                text,
                state,
                retained_output_refs,
                retained_subagent_ids,
                is_compaction_summary,
            } => {
                message
                    .retained_output_refs
                    .extend(retained_output_refs.iter().cloned());
                message
                    .retained_subagent_ids
                    .extend(retained_subagent_ids.iter().cloned());
                message.is_compaction_summary |= is_compaction_summary;
                message.content.push(ContentBlock::Text {
                    text: if *state == AssistantTextState::Padding {
                        EMPTY_RESPONSE_MARKER.into()
                    } else {
                        text.clone()
                    },
                });
            }
            HistoryItemKind::Reasoning {
                text,
                signature,
                redacted,
                ..
            } => {
                if *redacted {
                    message
                        .content
                        .push(ContentBlock::RedactedThinking { data: text.clone() });
                } else {
                    message
                        .content
                        .push(ContentBlock::thinking(text.clone(), signature.clone()));
                }
            }
            HistoryItemKind::ToolCall {
                call_id,
                name,
                input,
                thought_signature,
            } => message.content.push(ContentBlock::ToolUse {
                id: call_id.clone(),
                name: name.clone(),
                input: input.clone(),
                thought_signature: thought_signature.clone(),
            }),
            HistoryItemKind::ToolResult {
                call_id,
                content,
                is_error,
                output_ref,
                images,
            } => {
                message.content.push(ContentBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                    output_ref: output_ref.clone(),
                });
                result_image_index = message.content.len();
                result_images.extend(images.iter().cloned());
                message
                    .tool_result_image_owners
                    .extend(std::iter::repeat_n(call_id.clone(), images.len()));
            }
        }
    }

    message.content.splice(
        result_image_index..result_image_index,
        result_images
            .into_iter()
            .map(|source| ContentBlock::Image { source }),
    );
    message
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::types::ImageMediaType;

    const CALL_ONE: &str = "call-one";
    const CALL_TWO: &str = "call-two";
    const STORED_OUTPUT: &str = "first\nsecond";
    const TOOL_NAME: &str = "read";
    const REASONING_DURATION_MS: u64 = 9_700;

    fn image(data: &str) -> ImageSource {
        ImageSource::new(ImageMediaType::Png, Arc::from(data))
    }

    fn append_message(items: &mut Vec<HistoryItem>, message: &Message) {
        let parent_id = items.last().map(|item| item.id);
        items.extend(expand_message(message, parent_id));
    }

    fn item(kind: HistoryItemKind, group_id: CaudraId, parent_id: Option<CaudraId>) -> HistoryItem {
        HistoryItem {
            id: CaudraId::generate(),
            parent_id,
            group_id,
            kind,
        }
    }

    fn user_kind(text: &str) -> HistoryItemKind {
        HistoryItemKind::User {
            text: text.into(),
            images: Vec::new(),
            display_text: None,
            origin: UserOrigin::Turn,
        }
    }

    fn output_ref() -> ToolOutputRef {
        ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: STORED_OUTPUT.len(),
            line_count: STORED_OUTPUT.lines().count(),
        }
    }

    #[test]
    fn expands_and_projects_current_messages_losslessly() {
        let user = Message::user_display_with_images(
            "describe".into(),
            "describe image".into(),
            vec![image("user-image")],
        );
        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "inspect".into(),
                    signature: Some("reasoning-signature".into()),
                    duration_ms: Some(REASONING_DURATION_MS),
                },
                ContentBlock::Text {
                    text: "calling tool".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "redacted".into(),
                },
                ContentBlock::ToolUse {
                    id: CALL_ONE.into(),
                    name: TOOL_NAME.into(),
                    input: json!({"path": "a.rs"}),
                    thought_signature: Some("tool-signature".into()),
                },
            ],
            ..Default::default()
        };
        let result = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: CALL_ONE.into(),
                    content: "result".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Image {
                    source: image("tool-image"),
                },
            ],
            ..Default::default()
        };
        let messages = vec![user, assistant, result];
        let mut items = Vec::new();
        for message in &messages {
            append_message(&mut items, message);
        }

        assert!(
            items
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
        assert!(matches!(
            &items[0].kind,
            HistoryItemKind::User { text, images, .. }
                if text == "describe" && images.len() == 1
        ));
        assert!(matches!(
            items.last().map(|item| &item.kind),
            Some(HistoryItemKind::ToolResult { images, .. }) if images.len() == 1
        ));

        let projected = project_messages(&items).unwrap();
        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    #[test]
    fn tool_output_ref_roundtrips_through_persisted_history() {
        let output_ref = output_ref();
        let messages = [
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ONE.into(),
                    content: "result".into(),
                    is_error: false,
                    output_ref: Some(output_ref.clone()),
                }],
                ..Default::default()
            },
        ];
        let mut items = Vec::new();
        for message in &messages {
            append_message(&mut items, message);
        }
        let persisted = serde_json::to_value(items).unwrap();
        assert_eq!(persisted[1]["output_ref"]["id"], output_ref.id.to_string());
        let items: Vec<HistoryItem> = serde_json::from_value(persisted).unwrap();

        assert!(matches!(
            &items[1].kind,
            HistoryItemKind::ToolResult {
                output_ref: Some(reference),
                ..
            } if reference == &output_ref
        ));
        let projected = project_messages(&items).unwrap();
        assert!(matches!(
            &projected[1].content[0],
            ContentBlock::ToolResult {
                output_ref: Some(reference),
                ..
            } if reference == &output_ref
        ));
        assert!(
            serde_json::to_value(&projected[1]).unwrap()["content"][0]
                .get("output_ref")
                .is_none()
        );
    }

    #[test]
    fn reasoning_duration_persists_but_never_reaches_the_provider() {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "weighing options".into(),
                signature: None,
                duration_ms: Some(REASONING_DURATION_MS),
            }],
            ..Default::default()
        };

        let persisted = serde_json::to_value(expand_message(&message, None)).unwrap();
        assert_eq!(persisted[0]["duration_ms"], REASONING_DURATION_MS);
        let items: Vec<HistoryItem> = serde_json::from_value(persisted).unwrap();
        assert!(matches!(
            &items[0].kind,
            HistoryItemKind::Reasoning {
                duration_ms: Some(REASONING_DURATION_MS),
                ..
            }
        ));

        let projected = project_messages(&items).unwrap();
        assert!(matches!(
            &projected[0].content[0],
            ContentBlock::Thinking {
                duration_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn reasoning_without_duration_omits_the_key() {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::thinking("untimed".into(), None)],
            ..Default::default()
        };

        let persisted = serde_json::to_value(expand_message(&message, None)).unwrap();
        assert!(persisted[0].get("duration_ms").is_none());
        let items: Vec<HistoryItem> = serde_json::from_value(persisted).unwrap();
        assert!(matches!(
            &items[0].kind,
            HistoryItemKind::Reasoning {
                duration_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn compacted_host_metadata_roundtrips_without_public_serialization() {
        let output_ref = output_ref();
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("retained output ID: {}", output_ref.id),
            }],
            retained_output_refs: vec![output_ref.clone()],
            retained_subagent_ids: vec!["task-1".into()],
            is_compaction_summary: true,
            ..Default::default()
        };

        let items = expand_message(&message, None);
        let persisted = serde_json::to_value(&items).unwrap();
        assert_eq!(
            persisted[0]["retained_output_refs"][0]["id"],
            output_ref.id.to_string()
        );

        let items: Vec<HistoryItem> = serde_json::from_value(persisted).unwrap();
        let projected = project_messages(&items).unwrap();
        assert_eq!(projected[0].retained_output_refs, [output_ref]);
        assert_eq!(projected[0].retained_subagent_ids, ["task-1"]);
        assert!(projected[0].is_compaction_summary);
        assert!(
            serde_json::to_value(&projected[0])
                .unwrap()
                .get("retained_output_refs")
                .is_none()
        );
        assert!(
            serde_json::to_value(&projected[0])
                .unwrap()
                .get("retained_subagent_ids")
                .is_none()
        );
        assert!(
            serde_json::to_value(&projected[0])
                .unwrap()
                .get("is_compaction_summary")
                .is_none()
        );
    }

    #[test]
    fn expansion_associates_positional_result_images_atomically() {
        let message = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: CALL_ONE.into(),
                    content: "one".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: CALL_TWO.into(),
                    content: "two".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Image {
                    source: image("first"),
                },
                ContentBlock::Image {
                    source: image("second"),
                },
            ],
            ..Default::default()
        };
        let items = expand_message(&message, None);

        assert!(matches!(
            &items[0].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_ONE && &*images[0].data == "first"
        ));
        assert!(matches!(
            &items[1].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_TWO && &*images[0].data == "second"
        ));
    }

    #[test]
    fn expansion_preserves_sparse_first_result_image_owner() {
        let message = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: CALL_ONE.into(),
                    content: "one".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: CALL_TWO.into(),
                    content: "two".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::Image {
                    source: image("first"),
                },
            ],
            tool_result_image_owners: vec![CALL_ONE.into()],
            ..Default::default()
        };

        let items = expand_message(&message, None);

        assert!(matches!(
            &items[0].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_ONE && images.len() == 1 && &*images[0].data == "first"
        ));
        assert!(matches!(
            &items[1].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_TWO && images.is_empty()
        ));

        let mut history = Vec::new();
        append_message(
            &mut history,
            &Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({})),
                    ContentBlock::tool_use(CALL_TWO, TOOL_NAME, json!({})),
                ],
                ..Default::default()
            },
        );
        append_message(&mut history, &message);
        let projected = project_messages(&history).unwrap();
        assert_eq!(projected[1].tool_result_image_owners, [CALL_ONE]);
        let roundtrip = expand_message(&projected[1], None);
        assert!(matches!(
            &roundtrip[0].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_ONE && images.len() == 1 && &*images[0].data == "first"
        ));
        assert!(matches!(
            &roundtrip[1].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_TWO && images.is_empty()
        ));
    }

    #[test]
    fn expansion_preserves_user_origins_and_padding() {
        let messages = [
            Message::user("turn".into()),
            Message::observation("observation".into()),
            Message::synthetic("synthetic".into()),
            Message::empty_marker(),
        ];
        let origins: Vec<UserOrigin> = messages[..3]
            .iter()
            .map(|message| match &expand_message(message, None)[0].kind {
                HistoryItemKind::User { origin, .. } => *origin,
                _ => panic!("expected user item"),
            })
            .collect();
        assert_eq!(
            origins,
            [
                UserOrigin::Turn,
                UserOrigin::Observation,
                UserOrigin::Synthetic
            ]
        );
        assert!(matches!(
            expand_message(&messages[3], None)[0].kind,
            HistoryItemKind::AssistantText {
                state: AssistantTextState::Padding,
                ..
            }
        ));
    }

    #[test]
    fn projection_preserves_atomic_states_and_orders_result_images_last() {
        let assistant_group = CaudraId::generate();
        let result_group = CaudraId::generate();
        let mut items = Vec::new();
        let reasoning = item(
            HistoryItemKind::Reasoning {
                text: "partial".into(),
                signature: Some("signature".into()),
                redacted: false,
                interrupted: true,
                duration_ms: None,
            },
            assistant_group,
            None,
        );
        let first_call = item(
            HistoryItemKind::ToolCall {
                call_id: CALL_ONE.into(),
                name: TOOL_NAME.into(),
                input: json!({}),
                thought_signature: None,
            },
            assistant_group,
            Some(reasoning.id),
        );
        let second_call = item(
            HistoryItemKind::ToolCall {
                call_id: CALL_TWO.into(),
                name: TOOL_NAME.into(),
                input: json!({}),
                thought_signature: None,
            },
            assistant_group,
            Some(first_call.id),
        );
        let first_result = item(
            HistoryItemKind::ToolResult {
                call_id: CALL_ONE.into(),
                content: "one".into(),
                is_error: false,
                output_ref: None,
                images: vec![image("first")],
            },
            result_group,
            Some(second_call.id),
        );
        let second_result = item(
            HistoryItemKind::ToolResult {
                call_id: CALL_TWO.into(),
                content: "two".into(),
                is_error: true,
                output_ref: None,
                images: vec![image("second")],
            },
            result_group,
            Some(first_result.id),
        );
        items.extend([
            reasoning,
            first_call,
            second_call,
            first_result,
            second_result,
        ]);

        let messages = project_messages(&items).unwrap();
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::Thinking { thinking, signature, .. }
                if thinking == "partial" && signature.as_deref() == Some("signature")
        ));
        assert!(matches!(
            &messages[1].content[..],
            [
                ContentBlock::ToolResult { tool_use_id: first_id, .. },
                ContentBlock::ToolResult { tool_use_id: second_id, .. },
                ContentBlock::Image { source: first_image },
                ContentBlock::Image { source: second_image },
            ] if first_id == CALL_ONE
                && second_id == CALL_TWO
                && &*first_image.data == "first"
                && &*second_image.data == "second"
        ));
        assert_eq!(messages[1].tool_result_image_owners, [CALL_ONE, CALL_TWO]);
        let expanded = expand_message(&messages[1], None);
        assert!(matches!(
            &expanded[0].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_ONE && &*images[0].data == "first"
        ));
        assert!(matches!(
            &expanded[1].kind,
            HistoryItemKind::ToolResult { call_id, images, .. }
                if call_id == CALL_TWO && &*images[0].data == "second"
        ));
        assert!(
            serde_json::to_value(&messages[1])
                .unwrap()
                .get("tool_result_image_owners")
                .is_none()
        );
    }

    #[test]
    fn projection_rejects_duplicate_item_ids() {
        let first_group = CaudraId::generate();
        let second_group = CaudraId::generate();
        let first = item(user_kind("one"), first_group, None);
        let mut duplicate = item(user_kind("two"), second_group, Some(first.id));
        duplicate.id = first.id;

        assert!(matches!(
            project_messages(&[first, duplicate]),
            Err(HistoryProjectionError::DuplicateItemId { .. })
        ));
    }

    #[test]
    fn projection_rejects_noncontiguous_groups() {
        let first_group = CaudraId::generate();
        let second_group = CaudraId::generate();
        let first = item(user_kind("one"), first_group, None);
        let second = item(user_kind("two"), second_group, Some(first.id));
        let third = item(user_kind("three"), first_group, Some(second.id));

        assert!(matches!(
            project_messages(&[first, second, third]),
            Err(HistoryProjectionError::NoncontiguousGroup { group_id })
                if group_id == first_group
        ));
    }

    #[test]
    fn projection_rejects_mixed_group_kinds() {
        let group_id = CaudraId::generate();
        let user = item(user_kind("one"), group_id, None);
        let assistant = item(
            HistoryItemKind::AssistantText {
                text: "two".into(),
                state: AssistantTextState::Complete,
                retained_output_refs: Vec::new(),
                retained_subagent_ids: Vec::new(),
                is_compaction_summary: false,
            },
            group_id,
            Some(user.id),
        );

        assert!(matches!(
            project_messages(&[user, assistant]),
            Err(HistoryProjectionError::MixedGroupKinds { group_id: mixed })
                if mixed == group_id
        ));
    }

    #[test]
    fn projection_rejects_orphan_tool_results() {
        let result = item(
            HistoryItemKind::ToolResult {
                call_id: CALL_ONE.into(),
                content: "orphan".into(),
                is_error: false,
                output_ref: None,
                images: Vec::new(),
            },
            CaudraId::generate(),
            None,
        );

        assert!(matches!(
            project_messages(&[result]),
            Err(HistoryProjectionError::OrphanToolResult { call_id, .. })
                if call_id == CALL_ONE
        ));
    }

    #[test]
    fn projection_requires_the_complete_parallel_result_group() {
        let mut items = Vec::new();
        append_message(
            &mut items,
            &Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({})),
                    ContentBlock::tool_use(CALL_TWO, TOOL_NAME, json!({})),
                ],
                ..Default::default()
            },
        );
        append_message(
            &mut items,
            &Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ONE.into(),
                    content: "one".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        );

        assert!(matches!(
            project_messages(&items),
            Err(HistoryProjectionError::MissingToolResult { call_id, .. })
                if call_id == CALL_TWO
        ));
    }

    #[test]
    fn projection_rejects_duplicate_parallel_results() {
        let mut items = Vec::new();
        append_message(
            &mut items,
            &Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({})),
                    ContentBlock::tool_use(CALL_TWO, TOOL_NAME, json!({})),
                ],
                ..Default::default()
            },
        );
        append_message(
            &mut items,
            &Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: CALL_ONE.into(),
                        content: "one".into(),
                        is_error: false,
                        output_ref: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: CALL_ONE.into(),
                        content: "again".into(),
                        is_error: false,
                        output_ref: None,
                    },
                ],
                ..Default::default()
            },
        );

        assert!(matches!(
            project_messages(&items),
            Err(HistoryProjectionError::OrphanToolResult { call_id, .. })
                if call_id == CALL_ONE
        ));
    }

    #[test]
    fn projection_rejects_an_ordinary_group_between_calls_and_results() {
        let mut items = Vec::new();
        append_message(
            &mut items,
            &Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({}))],
                ..Default::default()
            },
        );
        append_message(&mut items, &Message::user("intervening".into()));

        assert!(matches!(
            project_messages(&items),
            Err(HistoryProjectionError::MissingToolResult { call_id, .. })
                if call_id == CALL_ONE
        ));
    }

    #[test]
    fn projection_allows_only_a_final_dangling_tool_call_group() {
        let mut dangling = Vec::new();
        append_message(
            &mut dangling,
            &Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(CALL_ONE, TOOL_NAME, json!({}))],
                ..Default::default()
            },
        );
        assert!(project_messages(&dangling).is_ok());

        append_message(
            &mut dangling,
            &Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "later".into(),
                }],
                ..Default::default()
            },
        );
        assert!(matches!(
            project_messages(&dangling),
            Err(HistoryProjectionError::MissingToolResult { .. })
        ));
    }

    #[test]
    fn projection_rejects_a_broken_parent_chain() {
        let first = item(user_kind("one"), CaudraId::generate(), None);
        let second = item(user_kind("two"), CaudraId::generate(), None);

        assert!(matches!(
            project_messages(&[first, second]),
            Err(HistoryProjectionError::InvalidParentLink { .. })
        ));
    }

    #[test]
    fn active_history_follows_parents_instead_of_storage_order() {
        let root = item(user_kind("root"), CaudraId::generate(), None);
        let abandoned = item(user_kind("abandoned"), CaudraId::generate(), Some(root.id));
        let branch = item(user_kind("branch"), CaudraId::generate(), Some(root.id));
        let items = vec![root.clone(), abandoned, branch.clone()];

        let active = active_history_items(&items, Some(branch.id)).unwrap();

        assert_eq!(active, [root, branch]);
    }

    #[test]
    fn merge_retains_abandoned_nodes_and_appends_the_new_branch() {
        let root = item(user_kind("root"), CaudraId::generate(), None);
        let abandoned = item(user_kind("abandoned"), CaudraId::generate(), Some(root.id));
        let branch = item(user_kind("branch"), CaudraId::generate(), Some(root.id));
        let mut stored = vec![root.clone(), abandoned.clone()];

        merge_history_items(&mut stored, &[root.clone(), branch.clone()]).unwrap();

        assert_eq!(stored, [root, abandoned, branch]);
    }

    #[test]
    fn active_history_rejects_missing_heads_and_parents() {
        let missing = CaudraId::generate();
        let orphan = item(user_kind("orphan"), CaudraId::generate(), Some(missing));

        assert!(matches!(
            active_history_items(&[], Some(missing)),
            Err(HistoryProjectionError::MissingHead { head_id }) if head_id == missing
        ));
        assert!(matches!(
            active_history_items(&[orphan], None),
            Err(HistoryProjectionError::MissingParent { parent_id, .. }) if parent_id == missing
        ));
    }

    #[test]
    fn legacy_head_defaults_to_the_last_item_but_empty_revert_does_not() {
        let root = item(user_kind("root"), CaudraId::generate(), None);

        assert_eq!(
            resolve_history_head(std::slice::from_ref(&root), None, false),
            Some(root.id)
        );
        assert_eq!(resolve_history_head(&[root], None, true), None);
    }
}
