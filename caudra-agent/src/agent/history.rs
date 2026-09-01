use std::borrow::Cow;
use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_providers::{
    CaudraId, ContentBlock, HistoryItem, HistoryItemKind,
    HistoryProjectionError, Message, Role, expand_message, project_messages,
};
use caudra_storage::sessions::next_epoch;
use tracing::warn;

const CANCEL_MARKER: &str = "[Cancelled by user]";
pub const UNAVAILABLE_RESULT: &str = "[Tool result not available]";

pub type HistorySnapshot = caudra_storage::sessions::HistorySnapshot<HistoryItem>;
pub type SharedHistory = Arc<ArcSwap<HistorySnapshot>>;

pub struct History {
    snapshot: HistorySnapshot,
    messages: Vec<Message>,
    mirror: Option<SharedHistory>,
}

impl History {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            snapshot: HistorySnapshot::new(expand_messages(&messages)),
            messages,
            mirror: None,
        }
    }

    pub fn restored(mut items: Vec<HistoryItem>) -> Result<Self, HistoryProjectionError> {
        let items_before = items.len();
        let changed = sanitize_restored_tool_items(&mut items)?;
        let messages = project_messages(&items)?;

        if changed {
            warn!(
                before = items_before,
                after = items.len(),
                "sanitized restored history"
            );
        }

        Ok(Self {
            snapshot: HistorySnapshot::new(items),
            messages,
            mirror: None,
        })
    }

    pub fn with_mirror(mut self, mirror: SharedHistory) -> Self {
        self.mirror = Some(mirror);
        self.publish();
        self
    }

    pub fn as_slice(&self) -> &[Message] {
        &self.messages
    }

    pub fn active_items(&self) -> &[HistoryItem] {
        &self.snapshot.messages
    }

    pub fn snapshot(&self) -> &HistorySnapshot {
        &self.snapshot
    }

    pub fn item_head(&self) -> Option<CaudraId> {
        self.active_items().last().map(|item| item.id)
    }

    pub fn push(&mut self, msg: Message) {
        self.extend([msg]);
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Skips system padding so repeated nudges cannot push tool results
    /// out of the window.
    pub fn has_recent_tool_results(&self, depth: usize) -> bool {
        self.as_slice()
            .iter()
            .rev()
            .filter(|m| !is_system_padding(m))
            .take(depth)
            .any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
    }

    /// Reads the padding tail instead of keeping a counter, so any real
    /// message resets the nudge budget on its own.
    pub fn recent_nudges(&self) -> u32 {
        self.as_slice()
            .iter()
            .rev()
            .take_while(|m| is_system_padding(m))
            .filter(|m| is_empty_marker(m))
            .count() as u32
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.snapshot = HistorySnapshot {
            epoch: next_epoch(),
            messages: Arc::new(expand_messages(&messages)),
        };
        self.messages = messages;
        self.publish();
    }

    pub fn truncate(&mut self, len: usize) {
        let item_len = item_len_for_message_count(self.active_items(), len);
        self.snapshot.epoch = next_epoch();
        Arc::make_mut(&mut self.snapshot.messages).truncate(item_len);
        self.messages.truncate(len);
        self.publish();
    }

    pub fn into_vec(self) -> Vec<Message> {
        self.messages
    }

    pub fn into_items(self) -> Vec<HistoryItem> {
        Arc::unwrap_or_clone(self.snapshot.messages)
    }

    fn extend(&mut self, messages: impl IntoIterator<Item = Message>) {
        let items = Arc::make_mut(&mut self.snapshot.messages);
        for message in messages {
            append_message_items(items, &message);
            self.messages.push(message);
        }
        self.publish();
    }

    /// The mirror gets the items as they are. Closing dangling tool calls
    /// here used to make the snapshot as long as the real results that came
    /// next, so the log never saw them. Callers that need an API-valid list
    /// close the dangling calls on their own copy.
    fn publish(&self) {
        let Some(mirror) = &self.mirror else { return };
        mirror.store(Arc::new(self.snapshot.clone()));
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

fn expand_messages(messages: &[Message]) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    for message in messages {
        append_message_items(&mut items, message);
    }
    items
}

fn append_message_items(items: &mut Vec<HistoryItem>, message: &Message) {
    items.extend(expand_message(message, items.last().map(|item| item.id)));
}

fn item_len_for_message_count(items: &[HistoryItem], message_count: usize) -> usize {
    if message_count == 0 {
        return 0;
    }

    let mut groups = 0;
    let mut previous_group = None;
    for (index, item) in items.iter().enumerate() {
        if previous_group != Some(item.group_id) {
            groups += 1;
            previous_group = Some(item.group_id);
        }
        if groups > message_count {
            return index;
        }
    }
    items.len()
}

fn sanitize_restored_tool_items(
    items: &mut Vec<HistoryItem>,
) -> Result<bool, HistoryProjectionError> {
    match project_messages(items) {
        Ok(_) => {}
        Err(error) if error.is_sanitizable_tool_order_error() => {}
        Err(error) => return Err(error),
    }

    let mut sanitized = Vec::with_capacity(items.len());
    let mut start = 0;
    let mut pending_calls = Vec::new();
    let mut changed = false;

    while start < items.len() {
        let group_id = items[start].group_id;
        let end = items[start..]
            .iter()
            .position(|item| item.group_id != group_id)
            .map_or(items.len(), |offset| start + offset);
        let group = &items[start..end];
        let had_results = group
            .iter()
            .any(|item| matches!(item.kind, HistoryItemKind::ToolResult { .. }));
        if !pending_calls.is_empty() && !had_results {
            append_unavailable_results(&mut sanitized, CaudraId::generate(), &pending_calls);
            pending_calls.clear();
            changed = true;
        }

        if !had_results {
            sanitized.extend_from_slice(group);
            pending_calls = group
                .iter()
                .filter_map(|item| match &item.kind {
                    HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect();
            start = end;
            continue;
        }

        let expected_calls = std::mem::take(&mut pending_calls);
        let mut missing_calls = expected_calls.clone();
        let mut kept_result = false;
        let group_start = sanitized.len();

        for item in group {
            let keep = match &item.kind {
                HistoryItemKind::ToolResult { call_id, .. } => {
                    let valid = missing_calls
                        .iter()
                        .position(|expected| expected == call_id)
                        .is_some_and(|position| {
                            missing_calls.remove(position);
                            true
                        });
                    kept_result |= valid;
                    valid
                }
                _ => true,
            };
            if keep {
                sanitized.push(item.clone());
            } else {
                changed = true;
            }
        }

        if expected_calls.is_empty() && !kept_result {
            let group_items = sanitized.split_off(group_start);
            sanitized.extend(group_items.into_iter().filter_map(|mut item| {
                let HistoryItemKind::User { text, images, .. } = &mut item.kind else {
                    return Some(item);
                };
                images.clear();
                (!text.is_empty()).then_some(item)
            }));
        }
        if !missing_calls.is_empty() {
            append_unavailable_results(&mut sanitized, group[0].group_id, &missing_calls);
            changed = true;
        }
        start = end;
    }

    if !pending_calls.is_empty() {
        append_unavailable_results(&mut sanitized, CaudraId::generate(), &pending_calls);
        changed = true;
    }

    if changed {
        let mut parent_id = None;
        for item in &mut sanitized {
            item.parent_id = parent_id;
            parent_id = Some(item.id);
        }
        *items = sanitized;
    }
    Ok(changed)
}

fn append_unavailable_results(
    items: &mut Vec<HistoryItem>,
    group_id: CaudraId,
    call_ids: &[String],
) {
    items.extend(call_ids.iter().map(|call_id| HistoryItem {
        id: CaudraId::generate(),
        parent_id: None,
        group_id,
        kind: HistoryItemKind::ToolResult {
            call_id: call_id.clone(),
            content: UNAVAILABLE_RESULT.into(),
            is_error: true,
            output_ref: None,
            images: Vec::new(),
        },
    }));
}

pub(super) fn remove_orphaned_tool_results(messages: &mut Vec<Message>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < messages.len() {
        if !matches!(messages[i].role, Role::User) {
            i += 1;
            continue;
        }

        let valid_ids: Vec<String> = if i > 0 && matches!(messages[i - 1].role, Role::Assistant) {
            messages[i - 1]
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        } else {
            Vec::new()
        };

        let content_len = messages[i].content.len();
        let (mut had_results, mut kept_results) = (false, false);
        messages[i].content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                had_results = true;
                let keep = valid_ids.iter().any(|id| id == tool_use_id);
                kept_results |= keep;
                keep
            }
            _ => true,
        });
        if had_results && !kept_results {
            messages[i]
                .content
                .retain(|b| !matches!(b, ContentBlock::Image { .. }));
        }
        changed |= messages[i].content.len() != content_len;

        if messages[i].content.is_empty() {
            messages.remove(i);
            changed = true;
        } else {
            i += 1;
        }
    }

    changed
}

pub(super) fn repair_tool_pairs(mut messages: Cow<'_, [Message]>) -> Cow<'_, [Message]> {
    if !tool_pairs_need_repair(&messages) {
        return messages;
    }

    let repaired = messages.to_mut();
    remove_orphaned_tool_results(repaired);
    synthesize_missing_tool_results(repaired);
    messages
}

fn tool_pairs_need_repair(messages: &[Message]) -> bool {
    messages.iter().enumerate().any(|(index, message)| {
        message.content.iter().any(|block| match block {
            ContentBlock::ToolUse { id, .. } => !messages
                .get(index + 1)
                .is_some_and(|next| has_tool_result(next, id)),
            ContentBlock::ToolResult { tool_use_id, .. } => index
                .checked_sub(1)
                .and_then(|previous| messages.get(previous))
                .is_none_or(|previous| !has_tool_use(previous, tool_use_id)),
            _ => false,
        })
    })
}

fn has_tool_use(message: &Message, id: &str) -> bool {
    matches!(message.role, Role::Assistant)
        && message
            .tool_uses()
            .any(|(tool_use_id, _, _)| tool_use_id == id)
}

fn has_tool_result(message: &Message, id: &str) -> bool {
    matches!(message.role, Role::User)
        && message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
        })
}

fn synthesize_missing_tool_results(messages: &mut Vec<Message>) {
    let mut index = 0;
    while index < messages.len() {
        if !matches!(messages[index].role, Role::Assistant) {
            index += 1;
            continue;
        }

        let missing: Vec<String> = messages[index]
            .tool_uses()
            .filter(|(id, _, _)| {
                !messages
                    .get(index + 1)
                    .is_some_and(|next| has_tool_result(next, id))
            })
            .map(|(id, _, _)| id.to_owned())
            .collect();
        if missing.is_empty() {
            index += 1;
            continue;
        }

        let results = missing
            .into_iter()
            .map(|tool_use_id| ContentBlock::ToolResult {
                tool_use_id,
                content: UNAVAILABLE_RESULT.into(),
                is_error: true,
                output_ref: None,
            });
        if messages
            .get(index + 1)
            .is_some_and(|next| matches!(next.role, Role::User))
        {
            messages[index + 1].content.splice(0..0, results);
        } else {
            messages.insert(
                index + 1,
                Message {
                    role: Role::User,
                    content: results.collect(),
                    display_text: Some(String::new()),
                    ..Default::default()
                },
            );
        }
        index += 2;
    }
}

/// Empty markers and synthetic prompts (empty `display_text`) are
/// bookkeeping, not conversation.
fn is_system_padding(m: &Message) -> bool {
    is_empty_marker(m)
        || (m.display_text.as_deref() == Some("")
            && m.content
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. })))
}

/// Role and `display_text` are part of the shape: a user who types the marker
/// text verbatim writes a real message, and it has to break the nudge streak
/// like any other.
fn is_empty_marker(m: &Message) -> bool {
    m.is_empty_padding()
}

pub fn close_dangling_tool_calls(messages: &mut Vec<Message>, note: &str) {
    let Some(last) = messages.last() else { return };
    if !matches!(last.role, Role::Assistant) || !last.has_tool_calls() {
        return;
    }
    let error_results: Vec<ContentBlock> = last
        .tool_uses()
        .map(|(id, _, _)| ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content: note.to_owned(),
            is_error: true,
            output_ref: None,
        })
        .collect();
    messages.push(Message {
        role: Role::User,
        content: error_results,
        display_text: Some(String::new()),
        ..Default::default()
    });
}

pub(crate) fn sanitize_cancelled_history(history: &mut History, rollback_len: usize) {
    if history.len() <= rollback_len {
        return;
    }
    let mut tail = history.as_slice().last().cloned().into_iter().collect();
    close_dangling_tool_calls(&mut tail, CANCEL_MARKER);
    let mut additions = Vec::with_capacity(2);
    if tail.len() == 2 {
        additions.push(tail.pop().unwrap());
    }
    additions.push(Message::synthetic(CANCEL_MARKER.into()));
    history.extend(additions);
}

#[cfg(test)]
mod tests {
    use caudra_providers::{ContentBlock, Message, Role};
    use test_case::test_case;

    use super::*;

    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const GO: &str = "go";

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == CANCEL_MARKER));
    }

    fn make_tool_use_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|id| ContentBlock::tool_use(*id, "read", serde_json::json!({})))
                .collect(),
            ..Default::default()
        }
    }

    fn make_tool_result_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: "ok".into(),
                    is_error: false,
                    output_ref: None,
                })
                .collect(),
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    fn make_mirror() -> SharedHistory {
        Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()))
    }

    fn restore_messages(messages: Vec<Message>) -> History {
        History::restored(expand_messages(&messages)).unwrap()
    }

    fn snapshot_messages(snapshot: &HistorySnapshot) -> Vec<Message> {
        project_messages(&snapshot.messages).unwrap()
    }

    #[track_caller]
    fn extract_error_ids(msg: &Message) -> Vec<&str> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test_case(
        vec![Message::user("old".into())],
        1,
        1,
        false
        ; "no_new_messages_is_noop"
    )]
    #[test_case(
        vec![Message::user("hello".into())],
        0,
        2,
        true
        ; "user_only_appends_marker"
    )]
    #[test_case(
        vec![
            Message::user("hello".into()),
            Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: "hi".into() }], ..Default::default() },
        ],
        0,
        3,
        true
        ; "complete_turn_appends_marker"
    )]
    fn sanitize_cancelled_history_cases(
        messages: Vec<Message>,
        rollback_len: usize,
        expected_len: usize,
        expect_cancel_marker: bool,
    ) {
        let mut history = History::new(messages);
        sanitize_cancelled_history(&mut history, rollback_len);
        assert_eq!(history.len(), expected_len);
        if expect_cancel_marker {
            assert_ends_with_cancel_marker(&history);
        }
    }

    #[test]
    fn sanitize_dangling_tool_use_adds_error_results() {
        let mut history = History::new(vec![
            Message::user("hello".into()),
            make_tool_use_msg(&["t1", "t2"]),
        ]);
        sanitize_cancelled_history(&mut history, 0);

        assert_eq!(extract_error_ids(&history.as_slice()[2]), ["t1", "t2"]);
        assert_ends_with_cancel_marker(&history);
    }

    #[test]
    fn mirror_is_verbatim_and_epoch_tracks_appends() {
        let mirror = make_mirror();
        let mut history = History::new(Vec::new()).with_mirror(Arc::clone(&mirror));
        let append_epoch = mirror.load().epoch;

        for i in 0..10 {
            history.push(Message::user(format!("msg-{i}")));
            assert_eq!(mirror.load().messages.len(), i + 1);
            assert_eq!(mirror.load().epoch, append_epoch, "push is an append");
        }

        history.truncate(3);
        assert_eq!(mirror.load().messages.len(), 3);
        assert_ne!(
            mirror.load().epoch,
            append_epoch,
            "truncate is not an append"
        );

        history.push(make_tool_use_msg(&["t_final"]));
        assert_eq!(history.len(), 4);
        assert_eq!(
            mirror.load().messages.len(),
            4,
            "dangling tool_use is mirrored verbatim"
        );
    }

    #[test]
    fn append_preserves_item_ids_groups_and_parent_chain() {
        let mut history = History::new(vec![Message::user(GO.into())]);
        let root_id = history.item_head().unwrap();

        history.push(make_tool_use_msg(&["t1", "t2"]));
        let assistant_items = &history.active_items()[1..];
        let assistant_group = assistant_items[0].group_id;
        assert!(
            assistant_items
                .iter()
                .all(|item| item.group_id == assistant_group)
        );
        assert_eq!(assistant_items[0].parent_id, Some(root_id));
        let assistant_head = history.item_head().unwrap();

        history.push(make_tool_result_msg(&["t1", "t2"]));
        let result_items = &history.active_items()[3..];
        let result_group = result_items[0].group_id;
        assert_ne!(assistant_group, result_group);
        assert!(
            result_items
                .iter()
                .all(|item| item.group_id == result_group)
        );
        assert_eq!(result_items[0].parent_id, Some(assistant_head));
        assert_eq!(history.active_items()[0].id, root_id);
        assert!(
            history
                .active_items()
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
    }

    #[test]
    fn restored_history_preserves_stable_item_ids() {
        let original = History::new(vec![
            Message::user(GO.into()),
            text_msg(Role::Assistant, "done"),
        ]);
        let items = original.active_items().to_vec();
        let ids: Vec<CaudraId> = items.iter().map(|item| item.id).collect();

        let restored = History::restored(items).unwrap();

        assert_eq!(
            restored
                .active_items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(restored.item_head(), ids.last().copied());
    }

    #[test]
    fn truncate_stops_at_group_boundary_and_mints_epoch() {
        let mirror = make_mirror();
        let mut history = History::new(vec![
            Message::user(GO.into()),
            make_tool_use_msg(&["t1", "t2"]),
            make_tool_result_msg(&["t1", "t2"]),
        ])
        .with_mirror(Arc::clone(&mirror));
        let epoch = mirror.load().epoch;
        let expected_ids: Vec<CaudraId> = history.active_items()[..3]
            .iter()
            .map(|item| item.id)
            .collect();

        history.truncate(2);

        assert_ne!(mirror.load().epoch, epoch);
        assert_eq!(history.len(), 2);
        assert_eq!(
            history
                .active_items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(history.item_head(), expected_ids.last().copied());
    }

    #[test]
    fn close_dangling_tool_uses_appends_error_results() {
        let mut messages = vec![Message::user("go".into()), make_tool_use_msg(&["t1", "t2"])];
        close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);

        assert_eq!(messages.len(), 3);
        let closing = &messages[2];
        assert!(matches!(closing.role, Role::User));
        assert_eq!(extract_error_ids(closing), ["t1", "t2"]);
        assert_eq!(closing.display_text.as_deref(), Some(""));
    }

    #[test]
    fn close_dangling_is_noop_when_tool_result_already_present() {
        let mut messages = vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1"]),
        ];
        close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);
        assert_eq!(messages.len(), 3, "no extra closing after real result");
    }

    #[test]
    fn into_vec_returns_inner_messages() {
        let mirror = make_mirror();
        let history = History::new(vec![Message::user("go".into()), make_tool_use_msg(&["t1"])])
            .with_mirror(Arc::clone(&mirror));

        assert_eq!(mirror.load().messages.len(), 2);
        assert_eq!(history.into_vec().len(), 2);
    }

    /// Cancelling closes the open tool calls and marks the turn, all onto the
    /// end of the list, so the log can keep appending instead of rewriting.
    #[test]
    fn sanitize_cancelled_history_appends_onto_the_mirror() {
        let mirror = make_mirror();
        let mut history = History::new(vec![Message::user(GO.into()), make_tool_use_msg(&["t1"])])
            .with_mirror(Arc::clone(&mirror));
        let epoch = mirror.load().epoch;

        sanitize_cancelled_history(&mut history, 0);

        let snap = mirror.load();
        let messages = snapshot_messages(&snap);
        assert_eq!(snap.epoch, epoch, "cancel cleanup is a pure append");
        assert_eq!(messages.len(), history.len(), "mirror projects verbatim");
        assert_eq!(extract_error_ids(&messages[2]), ["t1"]);
        assert!(messages[2].content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, .. } if content == CANCEL_MARKER
        )));
        assert!(matches!(
            &messages[3].content[0],
            ContentBlock::Text { text } if text == CANCEL_MARKER
        ));
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    #[test_case(
        vec![make_tool_result_msg(&["t1"])],
        0
        ; "orphan_at_start_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            text_msg(Role::Assistant, "done"),
            make_tool_result_msg(&["orphan1", "orphan2"]),
        ],
        2
        ; "orphans_after_non_tool_assistant_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1", "t2"]),
            make_tool_result_msg(&["t1", "t2"]),
        ],
        3
        ; "valid_pairing_preserved"
    )]
    #[test_case(
        vec![Message::user("go".into()), make_tool_use_msg(&["t1"])],
        3
        ; "dangling_tool_use_closed_with_synthetic_result"
    )]
    fn sanitize_restored_cases(messages: Vec<Message>, expected_len: usize) {
        let history = restore_messages(messages);
        assert_eq!(history.len(), expected_len);
    }

    #[test]
    fn sanitize_restored_drops_image_when_all_results_orphaned() {
        let image_block = ContentBlock::Image {
            source: caudra_providers::ImageSource::new(
                caudra_providers::ImageMediaType::Png,
                std::sync::Arc::from("aGVsbG8="),
            ),
        };
        let mut orphaned = make_tool_result_msg(&["orphan"]);
        orphaned.content.push(image_block.clone());
        let history = restore_messages(vec![Message::user("go".into()), orphaned]);
        assert_eq!(history.len(), 1);

        // Chat-pasted image (no tool results) is untouched.
        let history = restore_messages(vec![Message {
            role: Role::User,
            content: vec![image_block],
            ..Default::default()
        }]);
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history.as_slice()[0].content[0],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn sanitize_restored_keeps_image_when_any_result_survives() {
        let mut msg = make_tool_result_msg(&["t1", "orphan"]);
        msg.content.push(ContentBlock::Image {
            source: caudra_providers::ImageSource::new(
                caudra_providers::ImageMediaType::Png,
                std::sync::Arc::from("aGVsbG8="),
            ),
        });
        msg.tool_result_image_owners.push("t1".into());
        let history = restore_messages(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            msg,
        ]);
        let content = &history.as_slice()[2].content;
        assert_eq!(content.len(), 2);
        assert!(matches!(
            &content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"
        ));
        assert!(matches!(content[1], ContentBlock::Image { .. }));
    }

    #[test]
    fn remove_orphaned_tool_results_reports_content_change() {
        let mut result = make_tool_result_msg(&["orphan"]);
        result.content.push(ContentBlock::Text {
            text: "keep me".into(),
        });
        let mut messages = vec![result];

        assert!(remove_orphaned_tool_results(&mut messages));
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0].content[..],
            [ContentBlock::Text { text }] if text == "keep me"
        ));
        assert!(!remove_orphaned_tool_results(&mut messages));
    }

    #[test]
    fn repair_tool_pairs_borrows_well_formed_history() {
        let messages = vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1"]),
        ];

        assert!(matches!(
            repair_tool_pairs(Cow::Borrowed(&messages)),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn repair_tool_pairs_preserves_thinking_and_closes_mid_history_call() {
        let mut assistant = make_tool_use_msg(&["t1"]);
        assistant.content.insert(
            0,
            ContentBlock::thinking("signed reasoning".into(), Some("signature".into())),
        );
        let assistant_before = serde_json::to_value(&assistant).unwrap();
        let messages = vec![
            Message::user("go".into()),
            assistant,
            Message::user("later".into()),
        ];

        let repaired = repair_tool_pairs(Cow::Borrowed(&messages));

        assert!(matches!(repaired, Cow::Owned(_)));
        assert_eq!(
            serde_json::to_value(&repaired[1]).unwrap(),
            assistant_before
        );
        assert!(matches!(
            &repaired[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: true,
                ..
            } if tool_use_id == "t1" && content == UNAVAILABLE_RESULT
        ));
        assert!(matches!(
            &repaired[2].content[1],
            ContentBlock::Text { text } if text == "later"
        ));
    }

    #[test]
    fn sanitize_restored_partial_orphan_keeps_matched_ids() {
        let history = restore_messages(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1", "t2"]),
        ]);
        let results: Vec<&str> = history.as_slice()[2]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["t1"]);
    }

    #[test]
    fn sanitize_restored_closes_calls_before_an_intervening_user_group() {
        let history = restore_messages(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            Message::user("later".into()),
        ]);

        assert_eq!(history.len(), 4);
        assert_eq!(extract_error_ids(&history.as_slice()[2]), ["t1"]);
        assert_eq!(history.as_slice()[3].user_text(), Some("later"));
        assert!(project_messages(history.active_items()).is_ok());
    }

    #[test]
    fn sanitize_restored_completes_partial_parallel_results_once() {
        let history = restore_messages(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1", "t2"]),
            make_tool_result_msg(&["t1"]),
        ]);

        let result_message = &history.as_slice()[2];
        let ids: Vec<_> = result_message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [("t1", false), ("t2", true)]);
        assert!(project_messages(history.active_items()).is_ok());
    }

    #[test_case(
        vec![Message::user("go".into())],
        0
        ; "no_tool_results"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            make_tool_result_msg(&["t1"]),
        ],
        1
        ; "recent_tool_result"
    )]
    #[test_case(
        vec![
            Message::user("old1".into()),
            Message::user("old2".into()),
            Message::user("old3".into()),
            Message::user("old4".into()),
            Message::user("old5".into()),
            make_tool_result_msg(&["t1"]),
        ],
        1
        ; "at_depth_boundary"
    )]
    #[test_case(
        vec![
            make_tool_result_msg(&["t1"]),
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::empty_marker(),
            Message::synthetic("continue".into()),
        ],
        1
        ; "padding_does_not_hide_tool_results"
    )]
    fn has_recent_tool_results(messages: Vec<Message>, depth: usize) {
        let history = History::new(messages);
        let result = if depth == 0 {
            history.has_recent_tool_results(0)
        } else {
            history.has_recent_tool_results(depth)
        };
        assert_eq!(result, depth > 0);
    }

    #[test_case(vec![], 0 ; "empty_history")]
    #[test_case(
        vec![
            make_tool_result_msg(&["t1"]),
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::empty_marker(),
        ],
        3
        ; "counts_markers_in_padding_tail"
    )]
    #[test_case(
        vec![
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::user("continue".into()),
        ],
        0
        ; "user_message_resets_streak"
    )]
    #[test_case(
        vec![
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            Message::user(caudra_providers::EMPTY_RESPONSE_MARKER.into()),
        ],
        0
        ; "user_typing_the_marker_text_resets_streak"
    )]
    fn recent_nudges(messages: Vec<Message>, expected: u32) {
        assert_eq!(History::new(messages).recent_nudges(), expected);
    }

    /// The writer thread serializes a snapshot while the user keeps typing, so
    /// a published snapshot must never move under it.
    #[test]
    fn published_snapshot_is_frozen_against_later_mutations() {
        let mirror = make_mirror();
        let mut history =
            History::new(vec![Message::user(FIRST.into())]).with_mirror(Arc::clone(&mirror));

        let after_new = mirror.load_full();
        history.push(Message::user(SECOND.into()));
        assert_eq!(after_new.messages.len(), 1);
        assert_eq!(snapshot_messages(&after_new)[0].user_text(), Some(FIRST));
        assert!(!Arc::ptr_eq(&after_new.messages, &mirror.load().messages));

        let after_push = mirror.load_full();
        history.truncate(1);
        assert_eq!(after_push.messages.len(), 2);
        assert_eq!(snapshot_messages(&after_push)[1].user_text(), Some(SECOND));
        assert!(!Arc::ptr_eq(&after_push.messages, &mirror.load().messages));
    }

    /// After a respawn the old run's messages must be gone from the mirror
    /// right away, not only once the new run pushes something.
    #[test]
    fn with_mirror_overwrites_previous_run_snapshot_immediately() {
        let mirror = make_mirror();
        let run1 = History::new(vec![Message::user(FIRST.into())]).with_mirror(Arc::clone(&mirror));
        let run1_epoch = mirror.load().epoch;
        drop(run1);

        let _run2 = History::new(vec![Message::user(SECOND.into()), Message::user(GO.into())])
            .with_mirror(Arc::clone(&mirror));

        let snap = mirror.load();
        let messages = snapshot_messages(&snap);
        assert_eq!(snap.messages.len(), 2);
        assert_eq!(messages[0].user_text(), Some(SECOND));
        assert_ne!(snap.epoch, run1_epoch, "run 2 is not an append onto run 1");
    }

    #[test]
    fn restored_mints_fresh_epoch_and_mirrors_sanitized_messages() {
        let mirror = make_mirror();
        let seed_epoch = mirror.load().epoch;

        let history = History::restored(expand_messages(&[
            Message::user(GO.into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["orphan"]),
        ]))
        .unwrap()
        .with_mirror(Arc::clone(&mirror));

        let snap = mirror.load();
        let messages = snapshot_messages(&snap);
        assert_ne!(snap.epoch, seed_epoch);
        assert!(
            Arc::ptr_eq(&snap.messages, &history.snapshot.messages),
            "mirror shares the sanitized buffer verbatim"
        );
        assert_eq!(snap.messages.len(), history.active_items().len());
        assert_eq!(extract_error_ids(&messages[2]), ["t1"]);
    }

    #[test]
    fn sanitize_cancelled_history_noop_publishes_nothing() {
        let mirror = make_mirror();
        let mut history =
            History::new(vec![Message::user(GO.into())]).with_mirror(Arc::clone(&mirror));
        let before = mirror.load_full();
        let rollback_len = history.len();

        sanitize_cancelled_history(&mut history, rollback_len);

        let after = mirror.load_full();
        assert_eq!(before.epoch, after.epoch);
        assert!(Arc::ptr_eq(&before.messages, &after.messages));
    }

    /// Compaction can swap the whole list for one of the same length, which a
    /// length-only check would miss and quietly corrupt the log.
    #[test]
    fn replace_mints_new_epoch_even_when_length_is_unchanged() {
        let mirror = make_mirror();
        let mut history =
            History::new(vec![Message::user(FIRST.into())]).with_mirror(Arc::clone(&mirror));
        let epoch = mirror.load().epoch;
        let original_id = history.item_head().unwrap();

        history.replace(vec![Message::user(SECOND.into())]);

        let snap = mirror.load();
        let messages = snapshot_messages(&snap);
        assert_ne!(snap.epoch, epoch);
        assert_eq!(snap.messages.len(), 1);
        assert_ne!(history.item_head(), Some(original_id));
        assert_eq!(messages[0].user_text(), Some(SECOND));
    }
}
