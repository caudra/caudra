//! Rebuilds display messages from stored sessions. Tool outputs get syntax
//! highlighted, missing outputs fall back to plain text from `ToolResult`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::app::tasks::{TaskOutcome, TaskStatus};
use crate::components::messages::{MessagesPanel, PromptProgress};
use crate::components::tool_display::append_annotation;
use crate::components::{DisplayMessage, DisplayRole, DisplaySource, ToolRole, ToolStatus};
use crate::markdown::truncate_output;

use crate::selection::Selection;
use caudra_agent::permissions::PermissionRequest;
use caudra_agent::tools::{FILE_WRITE_TOOL_NAME, ToolEffect, ToolInvocation, ToolRegistry};
use caudra_agent::types::QuestionEvent;
use caudra_agent::{
    AgentEvent, BufferSnapshot, SubagentProgress, ToolDoneEvent, ToolOutput, ToolStartEvent,
};
use caudra_config::{ToolOutputLines, UiConfig};
use caudra_lua::WinView;
use caudra_providers::{CaudraId, HistoryItem, HistoryItemKind, UserOrigin};
use caudra_storage::view::ViewMode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::repaint::{Cadence, Dirty};

pub(crate) const DONE_TEXT: &str = "Done!";
pub(crate) const ERROR_TEXT: &str = "Error";
pub(crate) const CANCELLED_TEXT: &str = "Cancelled";
/// One notice per streak: a wedged model can spend twenty nudges, and twenty
/// identical bubbles bury the conversation they are about.
const NUDGE_TEXT: &str = "Model stalled after tool calls, nudging...";

pub enum ChatEventResult {
    Continue,
    Done,
    QueueItemConsumed {
        id: caudra_agent::QueueItemId,
        text: String,
        image_count: usize,
    },
    QueueBatchConsumed {
        items: Vec<caudra_agent::QueueConsumedItem>,
    },
    Error(String),
    PermissionRequest(Box<PermissionRequest>),
    PermissionRequestResolved {
        request_id: String,
    },
    AuthRequired,
    Question(Box<QuestionEvent>),
}

pub struct Chat {
    pub name: String,
    pub cost: Option<f64>,
    pub context_size: u32,
    pub model_id: Option<String>,
    pending_turn_usage: Option<String>,
    messages_panel: MessagesPanel,
    /// The ending and the index of the bubble announcing it, so a later, better
    /// informed outcome can fix that bubble instead of appending a second one.
    finish: Option<(TaskOutcome, usize)>,
    /// `None` for the main chat, the subagent's `tool_use_id` otherwise. That
    /// is the handle `caudra.task` addresses a task by, see `app::tasks`.
    task_id: Option<Arc<str>>,
    parent_tool_use_id: Option<Arc<str>>,
}

impl Chat {
    pub fn new(
        name: String,
        ui_config: UiConfig,
        lua_event_handle: caudra_lua::EventHandle,
    ) -> Self {
        Self {
            name,
            cost: None,
            context_size: 0,
            model_id: None,
            pending_turn_usage: None,
            messages_panel: MessagesPanel::new(ui_config, lua_event_handle),
            finish: None,
            task_id: None,
            parent_tool_use_id: None,
        }
    }

    pub(crate) fn subagent(
        task_id: &str,
        name: String,
        ui_config: UiConfig,
        lua_event_handle: caudra_lua::EventHandle,
    ) -> Self {
        Self {
            task_id: Some(Arc::from(task_id)),
            ..Self::new(name, ui_config, lua_event_handle)
        }
    }

    pub(crate) fn task_id(&self) -> Option<&Arc<str>> {
        self.task_id.as_ref()
    }

    pub(crate) fn parent_tool_use_id(&self) -> Option<&Arc<str>> {
        self.parent_tool_use_id.as_ref()
    }

    pub(crate) fn set_parent_tool_use_id(&mut self, id: impl Into<Arc<str>>) {
        self.parent_tool_use_id = Some(id.into());
    }

    pub(crate) fn task_status(&self) -> TaskStatus {
        self.finish.map(|(outcome, _)| outcome).into()
    }

    pub(crate) fn task_outcome(&self) -> Option<TaskOutcome> {
        self.finish.map(|(outcome, _)| outcome)
    }

    pub(crate) fn resume(&mut self) {
        self.flush();
        if let Some((_, bubble)) = self.finish.take() {
            self.messages_panel.remove(bubble);
        }
    }

    pub fn set_pending_turn_usage(&mut self, usage: String) {
        self.pending_turn_usage = Some(usage);
    }

    pub(crate) fn set_restore_channel(&mut self, event_tx: Option<caudra_agent::EventSender>) {
        self.messages_panel.set_restore_channel(event_tx);
    }

    pub fn handle_event(&mut self, event: AgentEvent, plan_path: Option<&Path>) -> ChatEventResult {
        match event {
            AgentEvent::ThinkingDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.thinking_delta(&text);
            }
            AgentEvent::ThinkingBoundary => self.messages_panel.thinking_boundary(),
            AgentEvent::TextDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.text_delta(&text);
            }
            AgentEvent::ToolPending { id, name } => self.messages_panel.tool_pending(id, &name),
            AgentEvent::ToolInputDelta {
                id, preview, size, ..
            } => self.messages_panel.tool_input_preview(&id, preview, size),
            AgentEvent::ToolStart(e) => self.messages_panel.tool_start(*e),
            AgentEvent::ToolOutput { id, content } => {
                self.messages_panel.tool_output(&id, &content)
            }
            AgentEvent::BatchProgress(e) => {
                self.messages_panel.batch_progress(&e.id, e.index, e.entry)
            }
            AgentEvent::ToolDone(e) => {
                let plan_write = plan_path.filter(|pp| e.wrote_to(pp));
                let is_full_write = &*e.tool == FILE_WRITE_TOOL_NAME;
                self.messages_panel.tool_done(*e);
                if let Some(pp) = plan_write {
                    let content = if is_full_write {
                        std::fs::read_to_string(pp).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    self.messages_panel
                        .push(DisplayMessage::plan(content, pp.display().to_string()));
                }
            }
            AgentEvent::TurnComplete(_) => {}
            AgentEvent::GoalEvaluating { .. }
            | AgentEvent::GoalEvaluation { .. }
            | AgentEvent::GoalFinished { .. }
            | AgentEvent::GoalDeferred { .. }
            | AgentEvent::GoalLoopCap { .. }
            | AgentEvent::GoalTurnLimit { .. }
            | AgentEvent::GoalEvaluationFailed { .. }
            | AgentEvent::GoalClearedAfterError { .. } => {}
            AgentEvent::ToolResultsSubmitted { .. } => {
                if let Some(usage) = self.pending_turn_usage.take() {
                    self.messages_panel.set_turn_usage_on_last_tool(usage);
                }
            }
            AgentEvent::AutoCompacting => {
                self.messages_panel.flush();
                self.messages_panel.push(DisplayMessage::new(
                    DisplayRole::Assistant,
                    "Auto-compacting conversation...".into(),
                ));
            }
            AgentEvent::CompactionDone => {
                self.messages_panel.flush();
            }
            AgentEvent::QueueItemConsumed {
                id,
                text,
                image_count,
            } => {
                return ChatEventResult::QueueItemConsumed {
                    id,
                    text,
                    image_count,
                };
            }
            AgentEvent::QueueBatchConsumed { items } => {
                return ChatEventResult::QueueBatchConsumed { items };
            }
            AgentEvent::QueueDrained | AgentEvent::SessionTitle { .. } => {}
            AgentEvent::Retry { .. } | AgentEvent::SubagentProgress { .. } => {
                unreachable!("handled before handle_event")
            }
            AgentEvent::Done { .. } => {
                self.messages_panel.flush();
                return ChatEventResult::Done;
            }
            AgentEvent::Error { message } => {
                self.messages_panel.flush();
                return ChatEventResult::Error(message);
            }
            AgentEvent::PermissionRequest(request) => {
                return ChatEventResult::PermissionRequest(request);
            }
            AgentEvent::PermissionRequestResolved { request_id, .. } => {
                return ChatEventResult::PermissionRequestResolved { request_id };
            }
            AgentEvent::AuthRequired => {
                return ChatEventResult::AuthRequired;
            }
            AgentEvent::Question(event) => {
                return ChatEventResult::Question(event);
            }
            AgentEvent::ToolSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel.tool_snapshot(&id, snapshot, theme_gen);
            }
            AgentEvent::ToolHeaderSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel
                    .tool_header_snapshot(&id, snapshot, theme_gen);
            }
            AgentEvent::Nudge => {
                self.messages_panel.flush();
                if self.messages_panel.last_message_text() != NUDGE_TEXT {
                    self.messages_panel.push(DisplayMessage::new(
                        DisplayRole::Assistant,
                        NUDGE_TEXT.into(),
                    ));
                }
            }
            AgentEvent::SubagentHistory { .. } => {}
            AgentEvent::LiveToolBuf { id, body } => {
                self.messages_panel.register_live_buf(id, body);
            }
            AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            } => {
                self.messages_panel
                    .set_prompt_progress((processed < total).then_some(PromptProgress {
                        processed,
                        total,
                        cache,
                    }));
            }
        }
        ChatEventResult::Continue
    }

    pub fn scroll(&mut self, delta: i32) {
        self.messages_panel.scroll(delta);
    }

    pub fn set_scroll_top(&mut self, top: u16) {
        self.messages_panel.set_scroll_top(top);
    }

    pub fn half_page(&self) -> i32 {
        self.messages_panel.half_page()
    }

    pub fn win_view(&self) -> WinView {
        self.messages_panel.win_view()
    }

    pub fn auto_scroll(&self) -> bool {
        self.messages_panel.auto_scroll()
    }

    pub fn scroll_to_top(&mut self) {
        self.messages_panel.scroll_to_top();
    }

    pub fn enable_auto_scroll(&mut self) {
        self.messages_panel.enable_auto_scroll();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.messages_panel.scroll_to_segment(segment_index);
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.messages_panel.restore_scroll(scroll_top, auto_scroll);
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.messages_panel.set_highlight_segment(idx);
    }

    pub fn set_accent(&mut self, color: Color) {
        self.messages_panel.set_accent(color);
    }

    pub fn set_view(&mut self, view: ViewMode) {
        self.messages_panel.set_view(view);
    }

    pub fn tick(&mut self) -> Dirty {
        self.messages_panel.tick()
    }

    pub fn cadence(&self) -> Cadence {
        self.messages_panel.cadence()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.messages_panel.view(frame, area, has_selection);
    }

    pub fn scroll_top(&self) -> u16 {
        self.messages_panel.scroll_top()
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        self.messages_panel.segment_heights()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.messages_panel.segment_search_texts()
    }

    pub fn last_reply_source(&self) -> Option<String> {
        self.messages_panel.last_reply_source()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        self.messages_panel.extract_selection_text(sel, msg_area)
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) {
        self.messages_panel.handle_click(row, area);
    }

    pub(crate) fn update_hover(&mut self, row: u16, col: u16, area: Rect, known_task_target: bool) {
        self.messages_panel
            .update_hover(row, col, area, known_task_target);
    }

    pub(crate) fn clear_hover(&mut self) {
        self.messages_panel.clear_hover();
    }

    pub(crate) fn hovered_link(&self) -> Option<&str> {
        self.messages_panel.hovered_link()
    }

    pub(crate) fn terminal_links(&self) -> &[crate::markdown::TerminalLink] {
        self.messages_panel.terminal_links()
    }

    pub(crate) fn link_at(&self, row: u16, col: u16, area: Rect) -> Option<Arc<str>> {
        self.messages_panel.link_at(row, col, area)
    }

    pub(crate) fn pan_hovered_diagram(&mut self, delta: i32) -> bool {
        self.messages_panel.pan_hovered_diagram(delta)
    }

    pub(crate) fn pan_visible_diagram(&mut self, delta: i32) -> bool {
        self.messages_panel.pan_visible_diagram(delta)
    }

    #[cfg(test)]
    pub(crate) fn panned_diagram_count(&self) -> usize {
        self.messages_panel.panned_diagram_count()
    }

    pub fn tool_id_at(&self, row: u16, area: Rect) -> Option<&str> {
        self.messages_panel.tool_id_at(row, area)
    }

    pub fn source_at(&self, row: u16, area: Rect) -> Option<DisplaySource> {
        self.messages_panel.source_at(row, area)
    }

    pub fn last_assistant_source(&self) -> Option<DisplaySource> {
        self.messages_panel.last_assistant_source()
    }

    pub(crate) fn review_target(
        &self,
        source: DisplaySource,
    ) -> Option<crate::components::messages::ReviewTarget> {
        self.messages_panel.review_target(source)
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_header_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn stream_reset(&mut self) {
        self.messages_panel.stream_reset();
    }

    pub fn flush(&mut self) {
        self.messages_panel.flush();
    }

    pub fn cancel_in_progress(&mut self) {
        self.messages_panel.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.messages_panel.fail_in_progress_with_message(message);
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        self.messages_panel
            .fail_in_progress_except(message, excluded);
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages_panel.push(msg);
    }

    /// Ends the transcript with the bubble [`TaskOutcome::role`] picks. A chat
    /// only ever grows one ending, but a caller who knows more than the one who
    /// got here first rewrites it in place. See [`TaskOutcome::refines`].
    pub(crate) fn mark_finished(&mut self, outcome: TaskOutcome, text: &str) {
        if let Some((previous, bubble)) = self.finish {
            if outcome.refines(previous) {
                self.finish = Some((outcome, bubble));
                self.messages_panel
                    .replace(bubble, DisplayMessage::new(outcome.role(), text.into()));
            }
            return;
        }
        self.messages_panel.flush();
        let bubble = self
            .messages_panel
            .push(DisplayMessage::new(outcome.role(), text.into()));
        self.finish = Some((outcome, bubble));
    }

    pub fn is_finished(&self) -> bool {
        self.finish.is_some()
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.messages_panel.update_tool_summary(tool_id, summary);
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.messages_panel.update_tool_model(tool_id, model);
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.messages_panel.set_tool_turn_usage(tool_id, usage);
    }

    pub fn set_tool_progress(&mut self, tool_id: &str, report: SubagentProgress) {
        self.messages_panel.set_tool_progress(tool_id, report);
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.messages_panel.load_messages(msgs);
    }

    pub fn bind_sources(&mut self, messages: &[DisplayMessage]) {
        self.messages_panel.bind_sources(messages);
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages_panel
            .push(DisplayMessage::new(DisplayRole::User, text.into()));
    }

    /// Flush, push, and re-pin scroll in one shot to avoid
    /// the one-frame hop where the bubble briefly lands in the wrong row.
    pub fn show_user_message(&mut self, text: impl Into<String>) {
        self.flush();
        self.push_user_message(text);
        self.enable_auto_scroll();
    }

    pub fn show_user_messages(&mut self, messages: impl IntoIterator<Item = String>) {
        self.flush();
        for message in messages {
            self.push_user_message(message);
        }
        self.enable_auto_scroll();
    }

    pub fn shell_tool_start(&mut self, event: ToolStartEvent) {
        self.messages_panel.tool_start(event);
    }

    pub fn shell_tool_output(&mut self, id: &str, content: &str) {
        self.messages_panel.tool_output(id, content);
    }

    pub fn shell_tool_done(&mut self, event: ToolDoneEvent) {
        self.messages_panel.tool_done(event);
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages_panel.message_count()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages_panel.message_at(index)
    }

    #[cfg(test)]
    pub fn in_progress_count(&self) -> usize {
        self.messages_panel.in_progress_count()
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages_panel.last_message_text()
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages_panel.last_message_is_plan()
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages_panel.last_message_role()
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.messages_panel.streaming_text_is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.messages_panel.streaming_thinking_is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages_panel.tool_turn_usage(tool_id)
    }
}

pub fn history_to_display(
    items: &[HistoryItem],
    tool_outputs: &HashMap<String, Arc<ToolOutput>>,
    tool_output_lines: &ToolOutputLines,
) -> (Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>) {
    let results = build_tool_results_map(items);
    let mut display = Vec::new();
    let mut restore_items: Vec<caudra_lua::RestoreItem> = Vec::new();
    let mut displayed_user_groups = HashSet::new();
    for item in items {
        match &item.kind {
            HistoryItemKind::User { .. } => {
                if displayed_user_groups.insert(item.group_id)
                    && let Some((id, text)) = visible_user_text(items, item.group_id)
                {
                    let mut message = DisplayMessage::new(DisplayRole::User, text.to_owned());
                    message.source = Some(DisplaySource::User(id));
                    display.push(message);
                }
            }
            HistoryItemKind::AssistantText { text, .. } if !text.is_empty() => {
                let mut message = DisplayMessage::new(DisplayRole::Assistant, text.clone());
                message.source = Some(DisplaySource::AssistantText(item.id));
                display.push(message);
            }
            HistoryItemKind::Reasoning {
                text,
                redacted: false,
                duration_ms,
                responses,
                ..
            } if !text.is_empty() || responses.is_some() => {
                let mut message = DisplayMessage::new(DisplayRole::Thinking, text.clone());
                message.source = Some(DisplaySource::Reasoning(item.id));
                message.thinking_duration = duration_ms.map(Duration::from_millis);
                display.push(message);
            }
            HistoryItemKind::ToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let static_name = name.as_str();
                let reg = ToolRegistry::global();
                let entry = reg.get(name);
                let effect = entry.as_ref().map_or(ToolEffect::Unknown, |e| e.effect);
                let tool_call: Option<Box<dyn ToolInvocation>> =
                    entry.and_then(|entry| entry.try_parse(input));
                let summary = reg.resolve_header(name, input);
                let result = results.get(call_id.as_str());
                let (status, result_text) = result
                    .map(|result| {
                        let status = if result.is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Success
                        };
                        (status, Some(result.content))
                    })
                    .unwrap_or((ToolStatus::Success, None));
                let reconstructed = tool_outputs.get(call_id.as_str()).cloned();
                let (text, truncated_lines, tool_output, mut annotation) = build_loaded_tool(
                    static_name,
                    &summary,
                    reconstructed.clone(),
                    result_text,
                    tool_output_lines,
                );
                if let Some(tool_annotation) = tool_call
                    .as_deref()
                    .and_then(|call| call.start_annotation())
                {
                    append_annotation(&mut annotation, &tool_annotation);
                }
                let output = reconstructed
                    .as_ref()
                    .map(|output| output.as_text())
                    .or_else(|| result_text.map(str::to_owned))
                    .unwrap_or_default();
                let state = reconstructed
                    .as_ref()
                    .and_then(|output| output.state().cloned());
                let rust_rendered = reconstructed
                    .as_ref()
                    .is_some_and(|output| output.structured_display_text().is_some());
                if !rust_rendered {
                    restore_items.push(caudra_lua::RestoreItem {
                        tool: Arc::from(static_name),
                        tool_use_id: call_id.clone(),
                        output,
                        input: input.clone(),
                        is_error: status == ToolStatus::Error,
                        tool_output_lines: *tool_output_lines,
                        theme_gen: None,
                        clicks: Vec::new(),
                        state,
                        lua_provenance: reconstructed
                            .as_ref()
                            .and_then(|output| output.lua_provenance().cloned()),
                    });
                }
                display.push(DisplayMessage {
                    role: DisplayRole::Tool(Box::new(ToolRole {
                        id: call_id.clone(),
                        status,
                        name: static_name.into(),
                        effect,
                    })),
                    text,
                    source: Some(DisplaySource::ToolCall {
                        id: item.id,
                        result_id: result.map(|result| result.id),
                    }),
                    tool_input: tool_call
                        .as_deref()
                        .and_then(|call| call.start_input())
                        .map(Arc::new),
                    tool_raw_input: Some(Arc::new(input.clone())),
                    tool_output,
                    live_output: None,
                    annotation,
                    progress: None,
                    plan_path: None,
                    timestamp: None,
                    turn_usage: None,
                    truncated_lines,
                    render_snapshot: None,
                    render_header: None,
                    snapshot_theme_gen: 0,
                    reasoning_open: None,
                    thinking_duration: None,
                });
            }
            HistoryItemKind::AssistantText { .. }
            | HistoryItemKind::Reasoning { .. }
            | HistoryItemKind::ToolResult { .. } => {}
        }
    }
    (display, restore_items)
}

fn visible_user_text(items: &[HistoryItem], group_id: CaudraId) -> Option<(CaudraId, &str)> {
    let mut users = items.iter().filter(|item| item.group_id == group_id);
    let first = users
        .clone()
        .find(|item| matches!(item.kind, HistoryItemKind::User { .. }))?;
    let HistoryItemKind::User {
        display_text,
        origin: UserOrigin::Turn,
        ..
    } = &first.kind
    else {
        return None;
    };
    if let Some(display_text) = display_text {
        return (!display_text.is_empty()).then_some((first.id, display_text.as_str()));
    }
    users.find_map(|item| match &item.kind {
        HistoryItemKind::User { text, .. } if !text.trim().is_empty() => {
            Some((item.id, text.as_str()))
        }
        _ => None,
    })
}

/// Rebuilds a Lua restore request from the data retained by a tool row.
pub(crate) fn restore_item_for(
    msg: &DisplayMessage,
    tool_output_lines: caudra_config::ToolOutputLines,
    theme_gen: u64,
) -> Option<caudra_lua::RestoreItem> {
    let DisplayRole::Tool(role) = &msg.role else {
        return None;
    };
    let input = msg.tool_raw_input.as_deref()?;
    let stored = msg.tool_output.as_deref()?;
    if stored.structured_display_text().is_some() {
        return None;
    }
    let output = stored.as_text();
    let state = stored.state().cloned();
    Some(caudra_lua::RestoreItem {
        tool: role.name.clone(),
        tool_use_id: role.id.clone(),
        output,
        input: input.clone(),
        is_error: role.status == ToolStatus::Error,
        tool_output_lines,
        theme_gen: Some(theme_gen),
        clicks: Vec::new(),
        state,
        lua_provenance: stored.lua_provenance().cloned(),
    })
}

/// Mirrors the live `tool_done` path so restored sessions
/// look the same as streamed ones.
fn build_loaded_tool(
    tool: &str,
    summary: &str,
    reconstructed: Option<Arc<ToolOutput>>,
    result_text: Option<&str>,
    tool_output_lines: &ToolOutputLines,
) -> (String, usize, Option<Arc<ToolOutput>>, Option<String>) {
    match reconstructed {
        Some(output) => {
            let annotation = output.annotation();
            (summary.to_owned(), 0, Some(output), annotation)
        }
        None => {
            let result = result_text.unwrap_or("");
            let annotation = if !result.is_empty() {
                ToolOutput::Plain(result.into()).annotation()
            } else {
                None
            };
            if result.is_empty() {
                (summary.to_owned(), 0, None, annotation)
            } else {
                let tr = truncate_output(result, tool_output_lines.get(tool));
                (
                    format!("{}\n{}", summary, tr.kept),
                    tr.skipped,
                    None,
                    annotation,
                )
            }
        }
    }
}

struct ToolResultRef<'a> {
    id: CaudraId,
    content: &'a str,
    is_error: bool,
}

fn build_tool_results_map(items: &[HistoryItem]) -> HashMap<&str, ToolResultRef<'_>> {
    let mut map = HashMap::new();
    for item in items {
        if let HistoryItemKind::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } = &item.kind
        {
            map.insert(
                call_id.as_str(),
                ToolResultRef {
                    id: item.id,
                    content,
                    is_error: *is_error,
                },
            );
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::{
        AgentEvent, IndexLine, IndexLineSemantic, IndexOutput, IndexSourceRange, ToolDoneEvent,
        ToolOutput, ToolStartEvent,
    };
    use caudra_config::UiConfig;
    use caudra_providers::{ContentBlock, Message, Role};
    use test_case::test_case;

    fn tool_start(id: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            effect: ToolEffect::Unknown,
            tool: tool.into(),
            summary: String::new(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        }))
    }

    fn tool_done(id: &str, tool: &str, output: ToolOutput) -> AgentEvent {
        tool_done_with_written_path(id, tool, output, None)
    }

    fn tool_done_with_written_path(
        id: &str,
        tool: &str,
        output: ToolOutput,
        written_path: Option<String>,
    ) -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: id.into(),
            tool: tool.into(),
            output,
            is_error: false,
            annotation: None,
            written_path,
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        }))
    }

    fn write_output(path: &str) -> (ToolOutput, Option<String>) {
        (
            ToolOutput::Plain(format!("wrote 42 bytes to {path}").into()),
            Some(path.to_owned()),
        )
    }

    fn edit_output(path: &str) -> ToolOutput {
        ToolOutput::Diff {
            path: path.into(),
            before: String::new(),
            after: String::new(),
            summary: String::new(),
        }
    }

    fn empty_outputs() -> HashMap<String, Arc<ToolOutput>> {
        HashMap::new()
    }

    fn display_messages(
        messages: &[Message],
        outputs: &HashMap<String, Arc<ToolOutput>>,
    ) -> (Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>) {
        history_to_display(
            &crate::history_items(messages),
            outputs,
            &ToolOutputLines::default(),
        )
    }

    const MAIN_NAME: &str = "Main";
    const SUBAGENT_NAME: &str = "research";
    const TASK_ID: &str = "toolu_01";
    const USER_TEXT: &str = "one more thing";
    const REPLY_TEXT: &str = "on it";
    const INDEX_SKELETON: &str = "fns:\n  pub run() [2]";
    const INDEX_ANNOTATION: &str = "2 lines";

    fn chat() -> Chat {
        Chat::new(
            MAIN_NAME.into(),
            UiConfig::default(),
            caudra_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn subagent_chat() -> Chat {
        Chat::subagent(
            TASK_ID,
            SUBAGENT_NAME.into(),
            UiConfig::default(),
            caudra_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn end(chat: &mut Chat, outcome: TaskOutcome) {
        let text = match outcome {
            TaskOutcome::Killed => CANCELLED_TEXT,
            TaskOutcome::Error => ERROR_TEXT,
            TaskOutcome::Unknown | TaskOutcome::Done => DONE_TEXT,
        };
        chat.mark_finished(outcome, text);
    }

    fn text_delta(chat: &mut Chat, text: &str) {
        chat.handle_event(AgentEvent::TextDelta { text: text.into() }, None);
    }

    #[test]
    fn tool_lifecycle() {
        let mut chat = chat();
        chat.handle_event(tool_start("t1", "shell"), None);
        assert_eq!(chat.in_progress_count(), 1);

        chat.handle_event(
            tool_done("t1", "shell", ToolOutput::Plain("ok".into())),
            None,
        );
        assert_eq!(chat.in_progress_count(), 0);
    }

    #[test]
    fn plan_write_renders_file_content() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("w1", "file_write"), Some(plan_path.as_path()));
        let (output, wp) = write_output(plan_str);
        chat.handle_event(
            tool_done_with_written_path("w1", "file_write", output, wp),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        let last = chat.last_message_text();
        assert!(last.contains("# My Plan"));
    }

    #[test]
    fn plan_write_ignores_different_path() {
        let mut chat = chat();
        let plan_path = Path::new("/plans/123.md");
        chat.handle_event(tool_start("w1", "file_write"), Some(plan_path));
        let (output, wp) = write_output("src/main.rs");
        chat.handle_event(
            tool_done_with_written_path("w1", "file_write", output, wp),
            Some(plan_path),
        );
        assert!(!chat.last_message_is_plan());
    }

    #[test]
    fn plan_edit_shows_path_only() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("e1", "file_edit"), Some(plan_path.as_path()));
        chat.handle_event(
            tool_done("e1", "file_edit", edit_output(plan_str)),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        assert!(chat.last_message_text().is_empty());
    }

    #[test]
    fn history_skips_empty_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: String::new(),
            }],
            ..Default::default()
        }];
        assert!(display_messages(&msgs, &empty_outputs()).0.is_empty());
    }

    #[test]
    fn history_hides_observations_but_keeps_the_reply() {
        let msgs = vec![
            Message::observation("build failed".into()),
            Message::synthetic("internal nudge".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix it".into(),
                }],
                ..Default::default()
            },
        ];
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::Assistant);
        assert_eq!(display[0].text, "I will fix it");
    }

    #[test]
    fn history_sources_keep_atomic_ids_on_merged_tools() {
        let messages = vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::thinking("inspect".into(), None),
                    ContentBlock::Text {
                        text: "running".into(),
                    },
                    ContentBlock::tool_use("t1", "shell", serde_json::json!({"command": "true"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&messages);

        let display = history_to_display(&items, &empty_outputs(), &ToolOutputLines::default()).0;

        assert_eq!(display[0].source, Some(DisplaySource::User(items[0].id)));
        assert_eq!(
            display[1].source,
            Some(DisplaySource::Reasoning(items[1].id))
        );
        assert_eq!(
            display[2].source,
            Some(DisplaySource::AssistantText(items[2].id))
        );
        assert_eq!(
            display[3].source,
            Some(DisplaySource::ToolCall {
                id: items[3].id,
                result_id: Some(items[4].id),
            })
        );
    }

    fn tool_use_pair(
        tool: &str,
        input: serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", tool, input)],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: result.into(),
                    is_error,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ]
    }

    #[test_case(false, ToolStatus::Success ; "success")]
    #[test_case(true,  ToolStatus::Error   ; "error")]
    fn history_tool_result_status(is_error: bool, expected: ToolStatus) {
        let msgs = tool_use_pair(
            "shell",
            serde_json::json!({"command": "ls"}),
            "output",
            is_error,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert_eq!(display.len(), 1);
        assert!(matches!(&display[0].role, DisplayRole::Tool(t) if t.status == expected));
    }

    #[test]
    fn restored_native_index_keeps_rust_renderer_without_lua_restore() {
        let messages = tool_use_pair(
            "index",
            serde_json::json!({"path": "src/lib.rs"}),
            INDEX_SKELETON,
            false,
        );
        let output = ToolOutput::Index(IndexOutput::File {
            path: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            language: "rust".into(),
            skeleton: INDEX_SKELETON.into(),
            lines: vec![
                IndexLine {
                    output_line: 1,
                    text: "fns:".into(),
                    semantic: IndexLineSemantic::Section,
                    body: None,
                    source_range: None,
                },
                IndexLine {
                    output_line: 2,
                    text: "  pub run() [2]".into(),
                    semantic: IndexLineSemantic::Item,
                    body: Some("  pub run()".into()),
                    source_range: Some(IndexSourceRange {
                        start_line: 2,
                        end_line: 2,
                    }),
                },
            ],
            source_line_count: 2,
            parse_error: false,
            truncated: false,
            instructions: None,
            state: Some(serde_json::json!({"kind": "file"})),
        });
        let outputs = HashMap::from([("t1".into(), Arc::new(output))]);

        let (display, restores) = display_messages(&messages, &outputs);

        assert!(restores.is_empty());
        assert!(matches!(
            display[0].tool_output.as_deref(),
            Some(ToolOutput::Index(_))
        ));
        assert_eq!(display[0].annotation.as_deref(), Some(INDEX_ANNOTATION));
    }

    #[test]
    fn index_without_stored_output_falls_back_to_plain_text_and_a_lua_restore() {
        let messages = tool_use_pair(
            "index",
            serde_json::json!({"path": "src/lib.rs"}),
            INDEX_SKELETON,
            false,
        );

        let (display, restores) = display_messages(&messages, &empty_outputs());

        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains(INDEX_SKELETON));
        assert_eq!(restores.len(), 1);
        assert_eq!(restores[0].output, INDEX_SKELETON);
    }

    #[test]
    fn history_mixed_conversation() {
        let msgs = vec![
            Message::user("do something".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Sure, let me help.".into(),
                    },
                    ContentBlock::tool_use(
                        "t1",
                        "shell",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "hi".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                ..Default::default()
            },
        ];
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert_eq!(display.len(), 4);
        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[1].role, DisplayRole::Assistant);
        assert!(matches!(display[2].role, DisplayRole::Tool(_)));
        assert_eq!(display[3].role, DisplayRole::Assistant);
        assert_eq!(display[3].text, "Done!");
    }

    #[test]
    fn history_stored_output_variants_pass_through() {
        let variants: Vec<(&str, serde_json::Value, ToolOutput)> = vec![
            (
                "file_edit",
                serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
                ToolOutput::Diff {
                    path: "a".into(),
                    before: "x\n".into(),
                    after: "y\n".into(),
                    summary: "edited a".into(),
                },
            ),
            (
                "file_read",
                serde_json::json!({"path": "/src/main.rs"}),
                ToolOutput::ReadCode {
                    path: "/src/main.rs".into(),
                    start_line: 1,
                    lines: vec!["fn main() {}".into()],
                    total_lines: 1,
                    instructions: None,
                },
            ),
            (
                "file_grep",
                serde_json::json!({"pattern": "TODO"}),
                ToolOutput::GrepResult {
                    entries: vec![],
                    capped: None,
                },
            ),
            (
                "todo_write",
                serde_json::json!({"todos": []}),
                ToolOutput::Plain("Todos cleared".into()),
            ),
        ];
        for (tool_name, input_json, output) in variants {
            let discriminant = std::mem::discriminant(&output);
            let msgs = tool_use_pair(tool_name, input_json, "ok", false);
            let outputs = HashMap::from([("t1".into(), Arc::new(output))]);
            let display = display_messages(&msgs, &outputs).0;
            assert_eq!(
                std::mem::discriminant(display[0].tool_output.as_deref().unwrap()),
                discriminant,
                "stored {tool_name} output should pass through"
            );
        }
    }

    #[test]
    fn history_stored_write_has_annotation() {
        let write_output = ToolOutput::WriteCode {
            path: "/src/main.rs".into(),
            byte_count: 12,
            lines: vec!["fn main() {}".into()],
        };
        let msgs = tool_use_pair(
            "file_write",
            serde_json::json!({"path": "/src/main.rs", "content": "fn main() {}"}),
            "wrote 12 bytes",
            false,
        );
        let outputs = HashMap::from([("t1".into(), Arc::new(write_output))]);
        let display = display_messages(&msgs, &outputs).0;
        assert!(display[0].annotation.is_some());
    }

    #[test]
    fn history_bash_output_truncated() {
        let long_output = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let joined = long_output.join("\n");
        let msgs = tool_use_pair(
            "shell",
            serde_json::json!({"command": "cmd"}),
            &joined,
            false,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        let line_count = display[0].text.lines().count();
        assert!(
            line_count < long_output.len(),
            "output should be truncated, got {line_count} lines for {} input lines",
            long_output.len()
        );
    }
    #[test]
    fn history_no_stored_output_falls_back_to_plain_text() {
        let msgs = tool_use_pair(
            "file_read",
            serde_json::json!({"path": "/src/main.rs"}),
            "1: fn main() {}",
            false,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains("fn main"));
    }

    /// Restoring a session re-parses the stored call, so the command echo above
    /// a tool's output survives a reload. It used to be dropped outright.
    #[test]
    fn a_restored_tool_call_keeps_its_input_echo() {
        use caudra_agent::tools::registry::{
            ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult,
            ToolInvocation,
        };
        use caudra_agent::tools::{DescriptionContext, ToolContext};
        use serde_json::{Value, json};

        const TOOL: &str = "restore_echo_probe";
        const COMMAND: &str = "echo restored";

        struct Probe;
        struct Call(String);

        impl Tool for Probe {
            fn name(&self) -> &str {
                TOOL
            }
            fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
                std::borrow::Cow::Borrowed("")
            }
            fn schema(&self) -> Value {
                json!({ "type": "object" })
            }
            fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
                Ok(Box::new(Call(
                    input["command"].as_str().unwrap_or_default().to_owned(),
                )))
            }
        }

        impl ToolInvocation for Call {
            fn start_header(&self) -> HeaderFuture {
                HeaderFuture::Ready(HeaderResult::plain(String::new()))
            }
            fn start_input(&self) -> Option<caudra_agent::ToolInput> {
                Some(caudra_agent::ToolInput::Code {
                    language: "bash".into(),
                    code: self.0.clone(),
                })
            }
            fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
                Box::pin(async move {
                    ToolExecResult::from(Ok(ToolOutput::Plain(String::new().into())))
                })
            }
        }

        caudra_agent::tools::ToolRegistry::global()
            .register(
                Arc::new(Probe),
                caudra_agent::tools::registry::ToolSource::Native {
                    owner: "test".into(),
                    contract: TOOL.into(),
                    trusted: true,
                },
            )
            .expect("probe registers once");

        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call-1",
                TOOL,
                json!({ "command": COMMAND }),
            )],
            ..Default::default()
        }];
        let display = display_messages(&messages, &HashMap::new()).0;
        let tool = display
            .iter()
            .find(|msg| matches!(msg.role, DisplayRole::Tool(_)))
            .expect("the call is displayed");
        let Some(caudra_agent::ToolInput::Code { code, .. }) = tool.tool_input.as_deref() else {
            panic!("a restored call keeps its code echo");
        };
        assert_eq!(code, COMMAND);
    }

    #[test]
    fn history_to_display_thinking_blocks() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("reasoning".into(), None),
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::RedactedThinking { data: "x".into() },
            ],
            ..Default::default()
        }];
        let display = display_messages(&msgs, &HashMap::new()).0;
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert_eq!(display[0].text, "reasoning");
        assert_eq!(display[1].role, DisplayRole::Assistant);
    }

    #[test]
    fn history_to_display_keeps_opaque_reasoning_as_a_title_only_block() {
        let mut reasoning = ContentBlock::thinking(String::new(), None);
        let ContentBlock::Thinking { responses, .. } = &mut reasoning else {
            unreachable!();
        };
        *responses = Some(caudra_providers::ResponsesReasoning {
            item_id: "rs_1".into(),
            encrypted_content: Some("ciphertext".into()),
        });
        let display = display_messages(
            &[Message {
                role: Role::Assistant,
                content: vec![reasoning],
                ..Default::default()
            }],
            &HashMap::new(),
        )
        .0;

        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert!(display[0].text.is_empty());
    }

    const RESTORE_OUTPUT: &str = "rendered output";

    fn tool_msg_with_input(tool: &str) -> DisplayMessage {
        let mut msg = DisplayMessage::new(DisplayRole::User, String::new());
        msg.role = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".into(),
            effect: ToolEffect::Unknown,
            status: ToolStatus::Success,
            name: tool.into(),
        }));
        msg.tool_raw_input = Some(Arc::new(serde_json::json!({ "q": tool })));
        msg.tool_output = Some(Arc::new(ToolOutput::Plain(RESTORE_OUTPUT.into())));
        msg
    }

    const RESTORE_THEME_GEN: u64 = 7;

    #[test]
    fn restore_item_for_round_trips_fields() {
        let msg = tool_msg_with_input("shell");
        let item = restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN)
            .expect("tool message with input and output must produce a RestoreItem");
        assert_eq!(&*item.tool, "shell");
        assert_eq!(item.tool_use_id, "t1");
        assert!(!item.is_error);
        assert_eq!(item.output, RESTORE_OUTPUT);
        assert_eq!(item.theme_gen, Some(RESTORE_THEME_GEN));
        assert_eq!(item.input, serde_json::json!({ "q": "shell" }));
    }

    #[test]
    fn restore_item_for_skips_structured_outputs_rust_renders() {
        let mut msg = tool_msg_with_input("file_edit");
        msg.tool_output = Some(Arc::new(edit_output("/src/main.rs")));
        assert!(restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn history_structured_output_produces_no_restore_item() {
        let msgs = tool_use_pair(
            "file_edit",
            serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
            "edited a",
            false,
        );
        let outputs = HashMap::from([("t1".to_owned(), Arc::new(edit_output("a")))]);
        let (_, items) = display_messages(&msgs, &outputs);
        assert!(items.is_empty(), "Rust owns Diff rendering on restore");

        let (_, items) = display_messages(&msgs, &empty_outputs());
        assert_eq!(items.len(), 1, "text-only history still restores via Lua");
    }

    #[test]
    fn restore_item_for_returns_none_when_data_missing() {
        let tol = ToolOutputLines::default();

        let plain = DisplayMessage::new(DisplayRole::Assistant, "hi".into());
        assert!(restore_item_for(&plain, tol, RESTORE_THEME_GEN).is_none());

        let mut no_input = tool_msg_with_input("shell");
        no_input.tool_raw_input = None;
        assert!(restore_item_for(&no_input, tol, RESTORE_THEME_GEN).is_none());

        let mut no_output = tool_msg_with_input("shell");
        no_output.tool_output = None;
        assert!(restore_item_for(&no_output, tol, RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn compaction_done_flushes_streaming_buffers() {
        let mut chat = chat();

        chat.handle_event(AgentEvent::AutoCompacting, None);
        assert_eq!(chat.message_count(), 1);

        chat.handle_event(
            AgentEvent::TextDelta {
                text: "summary".into(),
            },
            None,
        );
        chat.handle_event(
            AgentEvent::ThinkingDelta {
                text: "thinking".into(),
            },
            None,
        );
        assert!(!chat.streaming_text_is_empty());
        assert!(!chat.streaming_thinking_is_empty());

        chat.handle_event(AgentEvent::CompactionDone, None);
        assert!(chat.streaming_text_is_empty());
        assert!(chat.streaming_thinking_is_empty());
        assert_eq!(chat.message_count(), 3);

        chat.handle_event(AgentEvent::TextDelta { text: "new".into() }, None);
        chat.flush();
        assert_eq!(chat.message_count(), 4);
        assert_eq!(chat.last_message_text(), "new");
    }

    /// The transcript keeps growing after the ending, since the subagent chat
    /// stays on screen while the parent turn talks on. A fix aimed at the last
    /// message would eat whatever landed in between.
    #[test]
    fn a_correction_rewrites_the_recorded_bubble_not_the_last_one() {
        let mut chat = chat();
        end(&mut chat, TaskOutcome::Unknown);
        let ending = chat.message_count() - 1;

        chat.show_user_message(USER_TEXT);
        text_delta(&mut chat, REPLY_TEXT);
        chat.flush();
        let before = chat.message_count();

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), before);
        let bubble = chat
            .message_at(ending)
            .expect("recorded ending still there");
        assert_eq!(bubble.text, ERROR_TEXT);
        assert_eq!(bubble.role, DisplayRole::Error);
        assert_eq!(
            chat.message_at(ending + 1).map(|m| &m.role),
            Some(&DisplayRole::User)
        );
        assert_eq!(
            chat.message_at(ending + 1).map(|m| m.text.as_str()),
            Some(USER_TEXT)
        );
        assert_eq!(chat.last_message_text(), REPLY_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Assistant));
    }

    /// The stream is flushed before the ending is pushed, so a reply still in
    /// the buffer keeps its place ahead of it and the recorded index points at
    /// the ending, not at the flushed text.
    #[test]
    fn a_pending_stream_lands_before_the_ending_bubble() {
        let mut chat = chat();
        text_delta(&mut chat, REPLY_TEXT);
        assert!(!chat.streaming_text_is_empty());

        end(&mut chat, TaskOutcome::Unknown);

        assert!(chat.streaming_text_is_empty());
        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(chat.last_message_text(), DONE_TEXT);

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(
            chat.message_at(0).map(|m| &m.role),
            Some(&DisplayRole::Assistant)
        );
        assert_eq!(chat.last_message_text(), ERROR_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Error));
    }

    /// Every order two endings can arrive in. Only the placeholder gives way,
    /// a verdict is never walked back, and no order grows a second bubble.
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done   ; "placeholder_settles_as_done")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Killed, CANCELLED_TEXT, DisplayRole::Error, TaskStatus::Error ; "placeholder_corrected_to_killed")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Error, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error ; "placeholder_corrected_to_error")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Error, DONE_TEXT, DisplayRole::Done, TaskStatus::Done     ; "verdict_survives_late_error")]
    #[test_case(TaskOutcome::Error, TaskOutcome::Done, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error  ; "verdict_survives_late_done")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done      ; "repeated_verdict")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Unknown, DONE_TEXT, DisplayRole::Done, TaskStatus::Done ; "repeated_placeholder")]
    fn a_second_ending_never_adds_a_bubble(
        first: TaskOutcome,
        second: TaskOutcome,
        text: &str,
        role: DisplayRole,
        status: TaskStatus,
    ) {
        let mut chat = chat();
        chat.show_user_message(USER_TEXT);
        end(&mut chat, first);
        let count = chat.message_count();

        end(&mut chat, second);

        assert_eq!(chat.message_count(), count);
        assert_eq!(chat.last_message_text(), text);
        assert_eq!(chat.last_message_role(), Some(&role));
        assert_eq!(chat.task_status(), status);
    }

    /// The shape `caudra.task.list()` serializes: the main chat is not a task,
    /// and a subagent is one from the moment it starts, working until an
    /// ending lands on it.
    #[test]
    fn only_a_subagent_reports_a_task_and_its_status() {
        let main = chat();
        assert!(main.task_id().is_none());
        assert!(!main.is_finished());

        let mut sub = subagent_chat();
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
        assert_eq!(sub.task_status(), TaskStatus::Working);
        assert!(!sub.is_finished());

        end(&mut sub, TaskOutcome::Error);

        assert!(sub.is_finished());
        assert_eq!(sub.task_status(), TaskStatus::Error);
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
    }
}
