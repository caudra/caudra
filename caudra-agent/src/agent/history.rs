use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_providers::{
    CaudraId, ContentBlock, HistoryItem, HistoryItemKind, HistoryProjectionError, Message, Role,
    UserOrigin, expand_message, project_messages,
};
use caudra_storage::sessions::next_epoch;
use tracing::warn;

use crate::types::{TodoItem, ToolDoneEvent, ToolOutput};

pub(crate) const CANCEL_MARKER: &str = "[Cancelled by user]";
/// Opens the marker a run that died mid-turn leaves behind. A prefix rather than a fixed
/// string because the marker names the failure, which is the whole reason it exists.
pub(crate) const RUN_FAILED_PREFIX: &str = "[Run failed: ";
const RUN_FAILED_SUFFIX: &str = "]";
const MARKER_REASON_CHARS: usize = 200;
pub const UNAVAILABLE_RESULT: &str = "[Tool result not available]";

pub type HistorySnapshot = caudra_storage::sessions::HistorySnapshot<HistoryItem>;
pub type SharedHistory = Arc<ArcSwap<HistorySnapshot>>;

pub struct History {
    snapshot: HistorySnapshot,
    messages: Vec<Message>,
    mirror: Option<SharedHistory>,
    /// What every compaction replaced, oldest first. Never part of a request:
    /// it exists for readers that need what the user said before a seam,
    /// which the request deliberately no longer carries.
    archived: Vec<HistoryItem>,
    /// The plan as the last committed todo update left it. The model only ever
    /// saw "ok" for one, so this is the one place the list survives. `None`
    /// until an update lands, which is not the same as a list emptied by one.
    todos: Option<Vec<TodoItem>>,
}

impl History {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            snapshot: HistorySnapshot::new(expand_messages(&messages)),
            messages,
            mirror: None,
            archived: Vec::new(),
            todos: None,
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
            archived: Vec::new(),
            todos: None,
        })
    }

    pub fn with_mirror(mut self, mirror: SharedHistory) -> Self {
        self.mirror = Some(mirror);
        self.publish();
        self
    }

    /// Seeds the archive with what earlier sessions' compactions replaced, so
    /// a resumed session reads back past its seams the same as a live one.
    pub fn with_archived(mut self, items: Vec<HistoryItem>) -> Self {
        self.archived = items;
        self
    }

    /// Seeds the plan a restored session last committed; see [`stored_todos`].
    pub fn with_todos(mut self, todos: Option<Vec<TodoItem>>) -> Self {
        self.todos = todos;
        self
    }

    pub fn todos(&self) -> Option<&[TodoItem]> {
        self.todos.as_deref()
    }

    /// Keeps the list the last todo update among `results` set. Called with
    /// the results about to be committed, while their typed outputs still
    /// exist: committing reduces each to the text the model reads.
    pub(crate) fn record_todos(&mut self, results: &[ToolDoneEvent]) {
        if let Some(items) = results
            .iter()
            .rev()
            .find_map(|done| done.output.todo_update(done.is_error))
        {
            self.todos = Some(items.to_vec());
        }
    }

    /// Every item a reader scrolls: the archive, then the active chain. Not a
    /// valid request; it crosses every compaction seam.
    pub fn transcript_items(&self) -> Vec<HistoryItem> {
        self.transcript().cloned().collect()
    }

    pub(super) fn transcript(&self) -> impl DoubleEndedIterator<Item = &HistoryItem> {
        self.archived.iter().chain(self.active_items())
    }

    /// Bumped by `replace` and `truncate` but not by `extend`, so a caller can
    /// tell a wholesale swap of the conversation from an append. Compaction and
    /// conversation revert are the swaps, and both cool the message cache.
    pub fn epoch(&self) -> u64 {
        self.snapshot.epoch
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

    /// Reads the padding tail instead of keeping a counter, so the episode
    /// survives a restore and a fresh agent without new state.
    ///
    /// Only a response that carried work ends it. A message typed into a stall
    /// is what the stall interrupted, not evidence that the model recovered,
    /// and refilling the budget on it is what left a user unable to break out.
    pub fn recent_nudges(&self) -> u32 {
        self.as_slice()
            .iter()
            .rev()
            .take_while(|m| !is_productive_response(m))
            .filter(|m| is_empty_marker(m))
            .count() as u32
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.replace_superseding(messages, None);
    }

    /// Swaps the conversation and records the last item the new root replaced.
    /// Compaction is the only caller with something to record: it deliberately
    /// starts a chain the request reads instead of the turns behind it, and
    /// without this the transcript has no way back to them.
    pub fn replace_superseding(&mut self, messages: Vec<Message>, supersedes: Option<CaudraId>) {
        let mut items = expand_messages(&messages);
        if let Some(end) =
            supersedes.and_then(|id| self.snapshot.messages.iter().position(|item| item.id == id))
        {
            let (replaced, originals) = self.snapshot.messages.split_at(end + 1);
            stand_for_originals(&mut items, &replaced[end], originals);
            self.archived.extend_from_slice(replaced);
        }
        if let Some(root) = items.first_mut() {
            root.supersedes = supersedes;
        }
        self.snapshot = HistorySnapshot {
            epoch: next_epoch(),
            messages: Arc::new(items),
        };
        self.messages = messages;
        self.publish();
    }

    /// The last item covered by the first `message_count` messages, which is
    /// what a compaction records as superseded before it swaps the list.
    pub fn item_at_message_boundary(&self, message_count: usize) -> Option<CaudraId> {
        let items = self.active_items();
        item_len_for_message_count(items, message_count)
            .checked_sub(1)
            .and_then(|index| items.get(index))
            .map(|item| item.id)
    }

    pub fn truncate(&mut self, len: usize) {
        let item_len = item_len_for_message_count(self.active_items(), len);
        self.snapshot.epoch = next_epoch();
        Arc::make_mut(&mut self.snapshot.messages).truncate(item_len);
        self.messages.truncate(len);
        self.publish();
    }

    /// Undoes the tail [`sanitize_cancelled_history`] leaves behind, reporting
    /// whether there was one. The marker only exists so a cancelled turn ends
    /// on a user message; the cut itself is already recorded in the tool result
    /// or in the reply, so a resume drops it and the model picks the loop back
    /// up instead of answering the cancellation.
    ///
    /// No epoch bump, unlike [`Self::truncate`]: the epoch marks a wholesale
    /// swap of the conversation, and this is the exact inverse of the append
    /// that added the marker.
    pub(crate) fn drop_run_marker(&mut self) -> bool {
        if !self.messages.last().is_some_and(is_run_marker) {
            return false;
        }
        let item_len = item_len_for_message_count(self.active_items(), self.messages.len() - 1);
        Arc::make_mut(&mut self.snapshot.messages).truncate(item_len);
        self.messages.pop();
        self.publish();
        true
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

/// The todo list a stored transcript last committed, read from the typed
/// outputs its session kept by call ID.
///
/// A session keeps one output per ID, the newest. An older result under a
/// reused ID is therefore unknown, and the scan gives up there rather than
/// reach past it for a list that may since have been replaced.
pub fn stored_todos<'i, 'o>(
    transcript: impl DoubleEndedIterator<Item = &'i HistoryItem>,
    output: impl Fn(&str) -> Option<&'o ToolOutput>,
) -> Option<Vec<TodoItem>> {
    let mut seen = HashSet::new();
    for item in transcript.rev() {
        let HistoryItemKind::ToolResult {
            call_id, is_error, ..
        } = &item.kind
        else {
            continue;
        };
        if !seen.insert(call_id.as_str()) {
            return None;
        }
        if let Some(items) = output(call_id).and_then(|output| output.todo_update(*is_error)) {
            return Some(items.to_vec());
        }
    }
    None
}

/// Places a compaction's new chain in time. The kept turns come back as copies
/// behind the summary, and each is matched to the original it replaces from
/// the end, past thinking the compaction stripped and past results the repair
/// step added. What precedes the copies stands for the `superseded` item. Once
/// a copy cannot be matched, nothing ahead of it can be told apart from a copy,
/// so each of those points at itself.
fn stand_for_originals(
    items: &mut [HistoryItem],
    superseded: &HistoryItem,
    mut originals: &[HistoryItem],
) {
    let mut unplaced = items.len();
    while let (Some((original, earlier)), Some(index)) =
        (originals.split_last(), unplaced.checked_sub(1))
    {
        let item = &mut items[index];
        if original.kind == item.kind {
            item.stands_for = Some(original.happened_at().unwrap_or(item.id));
            unplaced = index;
            originals = earlier;
        } else if matches!(original.kind, HistoryItemKind::Reasoning { .. }) {
            originals = earlier;
        } else if added_by_repair(&item.kind) {
            unplaced = index;
        } else {
            break;
        }
    }
    let lead = superseded.happened_at().filter(|_| {
        originals
            .iter()
            .all(|original| matches!(original.kind, HistoryItemKind::Reasoning { .. }))
    });
    for item in &mut items[..unplaced] {
        item.stands_for = Some(lead.unwrap_or(item.id));
    }
}

/// A result the repair step closed a dangling call with, or the empty turn it
/// inserted to carry one.
fn added_by_repair(kind: &HistoryItemKind) -> bool {
    match kind {
        HistoryItemKind::ToolResult { .. } => true,
        HistoryItemKind::User {
            text,
            images,
            origin: UserOrigin::Synthetic,
            ..
        } => text.is_empty() && images.is_empty(),
        _ => false,
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
        supersedes: None,
        stands_for: None,
        group_id,
        kind: HistoryItemKind::ToolResult {
            call_id: call_id.clone(),
            content: UNAVAILABLE_RESULT.into(),
            is_error: true,
            output_ref: None,
            images: Vec::new(),
            refused_calls: Vec::new(),
            documents: Vec::new(),
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

/// A message the user actually sent, rather than an observation, a synthetic
/// nudge, or the carrier a tool result rides in. Compaction cuts the preserved
/// tail here and the provider projection starts protecting here, because both
/// need a boundary that leaves every tool call paired with its result.
pub(super) fn is_user_turn(message: &Message) -> bool {
    matches!(message.role, Role::User)
        && !message.is_observation()
        && message.display_text.as_deref() != Some("")
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Empty markers and synthetic prompts (empty `display_text`) are
/// bookkeeping, not conversation.
fn is_system_padding(m: &Message) -> bool {
    is_empty_marker(m)
        || m.steering.is_some()
        || (m.display_text.as_deref() == Some("")
            && m.content
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. })))
}

fn is_empty_marker(m: &Message) -> bool {
    m.is_empty_padding()
}

/// An assistant turn that carried a tool call or text a person could read.
/// Reasoning alone does not count: a turn that only thought is exactly the
/// turn the empty-response budget exists to answer.
fn is_productive_response(m: &Message) -> bool {
    matches!(m.role, Role::Assistant)
        && !is_empty_marker(m)
        && (m.has_tool_calls()
            || m.content.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty()),
            ))
}

/// The markers a run leaves behind when it ends without a reply: a cancel, or a failure that
/// killed the turn. Both exist only so the transcript ends on a user message, and both are
/// dropped by a resume so the model picks the loop back up.
///
/// Role and `display_text` are part of the shape here too: a user who types the marker text
/// verbatim wrote a real message, and a resume must not eat it.
fn is_run_marker(m: &Message) -> bool {
    matches!(m.role, Role::User)
        && m.display_text.as_deref() == Some("")
        && matches!(m.content.as_slice(), [ContentBlock::Text { text }]
            if text == CANCEL_MARKER || is_run_failure_marker(text))
}

/// Recognises what [`sanitize_failed_history`] wrote, so a frontend can tell a turn that died
/// mid-run from one that never started and offer the resume that picks it back up.
pub fn is_run_failure_marker(text: &str) -> bool {
    text.starts_with(RUN_FAILED_PREFIX) && text.ends_with(RUN_FAILED_SUFFIX)
}

fn close_dangling_tool_calls(messages: &mut Vec<Message>, note: &str) {
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

/// Ends a run that produced no reply on a user message, closing any tool call the loop left
/// dangling, so the transcript does not stop mid-turn and the next request has a seam to answer
/// from. Reports whether the marker was appended, so the caller can announce the same row the
/// restored transcript will draw from it.
fn close_run_with_marker(history: &mut History, rollback_len: usize, marker: &str) -> bool {
    if history.len() <= rollback_len {
        return false;
    }
    let mut tail = history.as_slice().last().cloned().into_iter().collect();
    close_dangling_tool_calls(&mut tail, marker);
    let mut additions = Vec::with_capacity(2);
    if tail.len() == 2 {
        additions.push(tail.pop().unwrap());
    }
    additions.push(Message::synthetic(marker.into()));
    history.extend(additions);
    true
}

pub(crate) fn sanitize_cancelled_history(history: &mut History, rollback_len: usize) -> bool {
    close_run_with_marker(history, rollback_len, CANCEL_MARKER)
}

/// Records a run killed by an error its retry budget could not absorb. Without it the transcript
/// ends on whatever the loop last wrote -- after a tool call, on the tool result -- and a
/// reloaded session looks like it simply stopped mid-task. Returns the marker it wrote so the
/// caller can announce the same text it persisted.
pub(crate) fn sanitize_failed_history(
    history: &mut History,
    rollback_len: usize,
    reason: &str,
) -> Option<String> {
    let marker = format!("{RUN_FAILED_PREFIX}{}{RUN_FAILED_SUFFIX}", one_line(reason));
    close_run_with_marker(history, rollback_len, &marker).then_some(marker)
}

/// A provider can fail with a whole response body. The marker is one row of transcript, so it
/// carries enough to recognise the failure and leaves the rest to the log.
fn one_line(reason: &str) -> String {
    let collapsed = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(MARKER_REASON_CHARS) {
        Some((end, _)) => format!("{}...", &collapsed[..end]),
        None => collapsed,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use caudra_providers::{ContentBlock, Message, Role, SteeringKind};
    use test_case::test_case;

    use super::*;
    use crate::types::{TodoPriority, TodoStatus};

    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const GO: &str = "go";
    const ANCHOR: &str = "anchor";
    /// The messages a compaction in these tests summarizes.
    const COMPACTED_HEAD: usize = 2;
    const FAILURE: &str = "inference engine is unavailable";
    const EMPTY_RULE: &str = "empty_response";
    const SPENT: &str = "a reply typed into a stall does not refill the budget";

    #[test_case(false; "live")]
    #[test_case(true; "canonical_round_trip")]
    fn steering_provenance_preserves_the_empty_episode(restored: bool) {
        let history = History::new(vec![
            make_tool_use_msg(&[FIRST]),
            make_tool_result_msg(&[FIRST]),
            Message::empty_marker(),
            Message::steering(SECOND.into(), EMPTY_RULE, SteeringKind::Recovery),
        ]);
        let mut history = if restored {
            History::restored(history.into_items()).unwrap()
        } else {
            history
        };
        assert_eq!(history.recent_nudges(), 1);
        assert!(history.has_recent_tool_results(1));
        history.push(Message::user(SECOND.into()));
        assert_eq!(history.recent_nudges(), 1, "{SPENT}");
        history.push(assistant_text(GO));
        assert_eq!(history.recent_nudges(), 0);
    }

    /// A compaction swaps the request's chain for a summary; the transcript
    /// still reads back to what the user said before it, across every seam.
    #[test]
    fn a_superseding_replace_archives_the_replaced_items() {
        let mut history = History::new(vec![
            Message::user(FIRST.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                ..Default::default()
            },
            Message::user(SECOND.into()),
        ]);
        let seam = history.item_at_message_boundary(2);
        history.replace_superseding(
            vec![
                Message::user("summary".into()),
                Message::user(SECOND.into()),
            ],
            seam,
        );
        let second_seam = history.item_at_message_boundary(1);
        history.replace_superseding(vec![Message::user(GO.into())], second_seam);

        let transcript = history.transcript_items();
        let texts: Vec<&str> = transcript
            .iter()
            .filter_map(|item| match &item.kind {
                HistoryItemKind::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, [FIRST, "summary", GO]);
        assert_eq!(history.active_items().len(), 1);
    }

    /// How a compaction's kept turns come back behind its summary.
    enum Replay {
        Verbatim,
        ThinkingStripped,
        ResultSynthesized,
        Edited,
        SyntheticText,
        SyntheticImage,
        Twice,
    }

    /// Where an item of the new chain happened, against the chain it replaced.
    #[derive(Debug, PartialEq)]
    enum Placed {
        At(usize),
        Itself,
        Unknown,
        Elsewhere,
    }

    fn compact(history: &mut History, kept: Vec<Message>) {
        let seam = history.item_at_message_boundary(COMPACTED_HEAD);
        let mut messages = vec![Message::user(ANCHOR.into()), assistant_text(COMPACTED)];
        messages.extend(kept);
        history.replace_superseding(messages, seam);
    }

    #[test_case(Replay::Verbatim => vec![Placed::At(1), Placed::At(1), Placed::At(2), Placed::At(3), Placed::At(4), Placed::At(5)] ; "copies_stand_for_their_originals")]
    #[test_case(Replay::ThinkingStripped => vec![Placed::At(1), Placed::At(1), Placed::At(2), Placed::At(4), Placed::At(5)] ; "stripped_thinking_is_passed_over")]
    #[test_case(Replay::ResultSynthesized => vec![Placed::At(1), Placed::At(1), Placed::At(2), Placed::At(3), Placed::At(4), Placed::At(5), Placed::Itself, Placed::Itself] ; "a_synthesized_result_stands_for_itself")]
    #[test_case(Replay::Edited => vec![Placed::Unknown, Placed::Unknown, Placed::Unknown, Placed::At(3), Placed::At(4), Placed::At(5)] ; "an_unmatched_copy_leaves_everything_ahead_unknown")]
    #[test_case(Replay::SyntheticText => vec![Placed::Unknown, Placed::Unknown, Placed::Unknown, Placed::At(3), Placed::At(4), Placed::At(5)] ; "a_synthetic_turn_with_text_is_no_repair")]
    #[test_case(Replay::SyntheticImage => vec![Placed::Unknown, Placed::Unknown, Placed::Unknown, Placed::At(3), Placed::At(4), Placed::At(5)] ; "a_synthetic_turn_with_an_image_is_no_repair")]
    #[test_case(Replay::Twice => vec![Placed::At(1), Placed::At(1), Placed::At(2), Placed::At(3), Placed::At(4), Placed::At(5)] ; "a_second_compaction_reaches_the_first_originals")]
    fn compaction_copies_happen_when_their_originals_did(replay: Replay) -> Vec<Placed> {
        let reply = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking(FIRST.into(), Some(SECOND.into())),
                ContentBlock::Text { text: GO.into() },
                ContentBlock::tool_use(PLAN_CALL, PLAN_CALL, serde_json::json!({})),
            ],
            ..Default::default()
        };
        let mut history = History::new(vec![
            Message::user(FIRST.into()),
            assistant_text(DRAFT),
            Message::user(SECOND.into()),
            reply.clone(),
        ]);
        let before: Vec<CaudraId> = history.active_items().iter().map(|item| item.id).collect();
        let stripped = Message {
            content: reply.content[1..].to_vec(),
            ..reply.clone()
        };
        let kept = match replay {
            Replay::Verbatim | Replay::Twice => vec![Message::user(SECOND.into()), reply],
            Replay::ThinkingStripped => vec![Message::user(SECOND.into()), stripped],
            Replay::ResultSynthesized => {
                repair_tool_pairs(Cow::Owned(vec![Message::user(SECOND.into()), reply]))
                    .into_owned()
            }
            Replay::Edited => vec![Message::user(SHIP.into()), reply],
            Replay::SyntheticText => vec![Message::synthetic(SHIP.into()), reply],
            Replay::SyntheticImage => vec![
                Message {
                    content: vec![png_block()],
                    ..Message::synthetic(String::new())
                },
                reply,
            ],
        };
        compact(&mut history, kept.clone());
        if matches!(replay, Replay::Twice) {
            compact(&mut history, kept);
        }
        history
            .active_items()
            .iter()
            .map(|item| match item.happened_at() {
                None => Placed::Unknown,
                Some(id) if id == item.id => Placed::Itself,
                Some(id) => before
                    .iter()
                    .position(|original| *original == id)
                    .map_or(Placed::Elsewhere, Placed::At),
            })
            .collect()
    }

    #[test]
    fn a_seeded_archive_precedes_the_active_chain() {
        let archived = History::new(vec![Message::user(FIRST.into())]).into_items();
        let history = History::new(vec![Message::user(GO.into())]).with_archived(archived);

        let transcript = history.transcript_items();

        assert_eq!(transcript.len(), 2);
        assert!(matches!(
            &transcript[0].kind,
            HistoryItemKind::User { text, .. } if text == FIRST
        ));
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == CANCEL_MARKER));
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
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
        let marked = sanitize_cancelled_history(&mut history, rollback_len);
        assert_eq!(history.len(), expected_len);
        assert_eq!(
            marked, expect_cancel_marker,
            "the report must match the marker, or the live row and the restored one disagree"
        );
        if expect_cancel_marker {
            assert_ends_with_cancel_marker(&history);
        }
    }

    /// The shape the outage produced: the loop got as far as a tool result and then the provider
    /// died. Without a marker the transcript ends there and a reload cannot say why.
    #[test]
    fn a_failed_run_closes_on_a_marker_naming_the_failure() {
        let mut history = History::new(vec![
            Message::user(GO.into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1"]),
        ]);

        let marker = sanitize_failed_history(&mut history, 1, FAILURE).expect("marker expected");

        assert!(
            marker.contains(FAILURE),
            "the marker must name the failure: {marker}"
        );
        assert_eq!(history.len(), 4);
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert_eq!(last.display_text.as_deref(), Some(""));
    }

    /// A resume has to land back on the tool result the loop stopped at, exactly as it does
    /// after a cancel, or the model answers the marker instead of continuing the task.
    #[test]
    fn a_failure_marker_is_dropped_by_a_resume() {
        let mut history = History::new(vec![
            Message::user(GO.into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1"]),
        ]);

        assert!(sanitize_failed_history(&mut history, 1, FAILURE).is_some());
        assert!(history.drop_run_marker());

        assert_eq!(history.len(), 3);
        assert!(matches!(
            history.as_slice().last().unwrap().content.as_slice(),
            [ContentBlock::ToolResult { .. }]
        ));
    }

    /// A run that wrote nothing has nothing to close, and a marker there would be a turn the
    /// user never took.
    #[test]
    fn a_failure_before_the_run_wrote_anything_marks_nothing() {
        let mut history = History::new(vec![Message::user(GO.into())]);

        assert!(sanitize_failed_history(&mut history, 1, FAILURE).is_none());
        assert_eq!(history.len(), 1);
    }

    /// A provider can fail with a whole response body, and the marker is one transcript row.
    #[test]
    fn a_failure_marker_stays_one_readable_line() {
        let sprawling = format!("API error (503):\n{}", "x".repeat(MARKER_REASON_CHARS * 2));
        let mut history = History::new(vec![Message::user(GO.into()), make_tool_use_msg(&["t1"])]);

        let marker = sanitize_failed_history(&mut history, 1, &sprawling).expect("marker expected");

        assert!(
            !marker.contains('\n'),
            "the marker must not break the row: {marker}"
        );
        assert!(marker.chars().count() < sprawling.chars().count());
        assert!(marker.starts_with(RUN_FAILED_PREFIX) && marker.ends_with(RUN_FAILED_SUFFIX));
    }

    /// A dangling call has to be closed before the marker, or the next request goes out with a
    /// tool call nothing answered.
    #[test]
    fn a_failure_inside_a_tool_call_closes_it_first() {
        let mut history = History::new(vec![Message::user(GO.into()), make_tool_use_msg(&["t1"])]);

        assert!(sanitize_failed_history(&mut history, 1, FAILURE).is_some());
        assert!(history.drop_run_marker());

        let last = history.as_slice().last().unwrap();
        assert_eq!(extract_error_ids(last), ["t1"]);
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

    /// The marker is the only tail a resume may eat, and it is recognised by
    /// shape rather than by text so a user who types it keeps their message.
    #[test_case(vec![Message::user(GO.into())], false ; "user_turn_stays")]
    #[test_case(vec![Message::user(CANCEL_MARKER.into())], false ; "typed_marker_stays")]
    #[test_case(
        vec![make_tool_use_msg(&["t1"]), make_tool_result_msg(&["t1"])],
        false
        ; "tool_result_stays"
    )]
    #[test_case(vec![text_msg(Role::Assistant, FIRST)], false ; "assistant_stays")]
    #[test_case(vec![], false ; "empty_history_is_noop")]
    #[test_case(
        vec![Message::user(GO.into()), Message::synthetic(CANCEL_MARKER.into())],
        true
        ; "synthetic_marker_is_dropped"
    )]
    fn drop_run_marker_cases(messages: Vec<Message>, expected: bool) {
        let mirror = make_mirror();
        let len = messages.len();
        let mut history = History::new(messages).with_mirror(Arc::clone(&mirror));
        let epoch = mirror.load().epoch;

        assert_eq!(history.drop_run_marker(), expected);

        let expected_len = len - usize::from(expected);
        assert_eq!(history.len(), expected_len);
        let snap = mirror.load();
        assert_eq!(snap.epoch, epoch, "dropping the marker is not a swap");
        assert_eq!(snapshot_messages(&snap).len(), expected_len);
    }

    /// Cancelling inside a tool call is what a resume has to land on: dropping
    /// the marker leaves the closing tool result, which is exactly where the
    /// loop would have carried on.
    #[test]
    fn cancel_then_resume_ends_on_the_closing_tool_result() {
        let mut history = History::new(vec![Message::user(GO.into()), make_tool_use_msg(&["t1"])]);

        sanitize_cancelled_history(&mut history, 0);
        assert!(history.drop_run_marker());

        assert_eq!(history.len(), 3);
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert_eq!(extract_error_ids(last), ["t1"]);
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

    fn png_block() -> ContentBlock {
        ContentBlock::Image {
            source: caudra_providers::ImageSource::new(
                caudra_providers::ImageMediaType::Png,
                std::sync::Arc::from("aGVsbG8="),
            ),
        }
    }

    #[test]
    fn sanitize_restored_drops_image_when_all_results_orphaned() {
        let image_block = png_block();
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
        msg.content.push(png_block());
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
        1
        ; "a_user_message_does_not_refill_the_budget"
    )]
    #[test_case(
        vec![
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            assistant_text(FIRST),
        ],
        0
        ; "a_productive_response_ends_the_episode"
    )]
    #[test_case(
        vec![
            Message::empty_marker(),
            Message::synthetic("nudge".into()),
            make_tool_use_msg(&[FIRST]),
        ],
        0
        ; "a_tool_call_ends_the_episode"
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

    const PLAN_CALL: &str = "plan";
    const REPLAN_CALL: &str = "replan";
    const READ_CALL: &str = "read";
    const DRAFT: &str = "draft the reminder";
    const SHIP: &str = "ship the reminder";
    const COMPACTED: &str = "summary of the work so far";
    const RESTORE_AGREES: &str = "a restored session must resume the plan the live one held";

    fn plan(content: &str) -> Vec<TodoItem> {
        vec![TodoItem {
            content: content.into(),
            status: TodoStatus::InProgress,
            priority: TodoPriority::High,
        }]
    }

    fn todo_done(call_id: &str, todos: Vec<TodoItem>, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            output: ToolOutput::TodoList(todos),
            is_error,
            ..ToolDoneEvent::error(call_id.into(), String::new())
        }
    }

    fn failed(call_id: &str) -> ToolDoneEvent {
        ToolDoneEvent::error(call_id.into(), FAILURE)
    }

    /// Commits `results` the way tool dispatch does, and keeps each typed
    /// output under its call ID the way a session store does.
    fn commit(
        history: &mut History,
        outputs: &mut HashMap<String, ToolOutput>,
        results: Vec<ToolDoneEvent>,
    ) {
        let ids: Vec<&str> = results.iter().map(|done| done.id.as_str()).collect();
        history.push(make_tool_use_msg(&ids));
        history.record_todos(&results);
        for done in &results {
            outputs.insert(done.id.clone(), done.output.clone());
        }
        history.push(crate::types::tool_results(results));
    }

    fn restored_todos(
        items: &[HistoryItem],
        outputs: &HashMap<String, ToolOutput>,
    ) -> Option<Vec<TodoItem>> {
        stored_todos(items.iter(), |call_id| outputs.get(call_id))
    }

    #[test_case(vec![vec![todo_done(PLAN_CALL, plan(DRAFT), false)]], Some(plan(DRAFT)) ; "an_update_sets_it")]
    #[test_case(
        vec![vec![todo_done(PLAN_CALL, plan(DRAFT), false)], vec![todo_done(REPLAN_CALL, plan(SHIP), false)]],
        Some(plan(SHIP)) ; "a_later_update_replaces_it"
    )]
    #[test_case(
        vec![vec![todo_done(PLAN_CALL, plan(DRAFT), false), todo_done(REPLAN_CALL, plan(SHIP), false)]],
        Some(plan(SHIP)) ; "the_last_update_of_one_response_wins"
    )]
    #[test_case(
        vec![vec![todo_done(PLAN_CALL, plan(DRAFT), false)], vec![todo_done(REPLAN_CALL, plan(SHIP), true)]],
        Some(plan(DRAFT)) ; "a_failed_update_keeps_it"
    )]
    #[test_case(
        vec![vec![todo_done(PLAN_CALL, plan(DRAFT), false)], vec![todo_done(REPLAN_CALL, Vec::new(), false)]],
        Some(Vec::new()) ; "an_empty_update_clears_it"
    )]
    #[test_case(vec![vec![failed(READ_CALL)]], None ; "no_update_leaves_it_unknown")]
    fn the_plan_is_the_last_successful_update(
        commits: Vec<Vec<ToolDoneEvent>>,
        expected: Option<Vec<TodoItem>>,
    ) {
        let mut history = History::new(vec![Message::user(GO.into())]);
        let mut outputs = HashMap::new();
        for results in commits {
            commit(&mut history, &mut outputs, results);
        }

        assert_eq!(history.todos(), expected.as_deref());
        assert_eq!(
            restored_todos(&history.transcript_items(), &outputs),
            expected,
            "{RESTORE_AGREES}"
        );
    }

    #[test]
    fn a_plan_committed_before_a_compaction_outlives_it() {
        let mut history = History::new(vec![Message::user(GO.into())]);
        let mut outputs = HashMap::new();
        commit(
            &mut history,
            &mut outputs,
            vec![todo_done(PLAN_CALL, plan(DRAFT), false)],
        );
        let seam = history.item_at_message_boundary(history.len());
        history.replace_superseding(vec![Message::user(COMPACTED.into())], seam);
        commit(&mut history, &mut outputs, vec![failed(READ_CALL)]);

        assert_eq!(history.todos(), Some(plan(DRAFT).as_slice()));
        assert_eq!(
            restored_todos(&history.transcript_items(), &outputs),
            Some(plan(DRAFT)),
            "{RESTORE_AGREES}"
        );
        assert_eq!(
            restored_todos(history.active_items(), &outputs),
            None,
            "the update lives behind the seam, so a restore has to read the archive"
        );
    }

    /// A session keeps one output per call ID, so a result under a reused ID
    /// hides whatever an older result under it said.
    #[test_case(false => None ; "an_update_overwritten_under_its_id_is_unknown")]
    #[test_case(true => Some(plan(SHIP)) ; "an_update_newest_under_its_id_is_kept")]
    fn a_reused_call_id_never_restores_a_plan_it_may_have_replaced(
        update_last: bool,
    ) -> Option<Vec<TodoItem>> {
        let mut history = History::new(vec![Message::user(GO.into())]);
        let mut outputs = HashMap::new();
        commit(
            &mut history,
            &mut outputs,
            vec![todo_done(PLAN_CALL, plan(DRAFT), false)],
        );
        let update = todo_done(REPLAN_CALL, plan(SHIP), false);
        let other = failed(REPLAN_CALL);
        let (first, second) = if update_last {
            (other, update)
        } else {
            (update, other)
        };
        commit(&mut history, &mut outputs, vec![first]);
        commit(&mut history, &mut outputs, vec![second]);

        assert_eq!(history.todos(), Some(plan(SHIP).as_slice()));
        restored_todos(&history.transcript_items(), &outputs)
    }
}
