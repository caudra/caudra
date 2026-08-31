mod layout;
mod render;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

use self::render::{EXPAND_AFFORDANCE, HoverFeedback, RenderCursor, RenderFeedback};
use self::segment::{Segment, SegmentCache, wrapped_line_count};
use layout::{SegmentChrome, SegmentKind};

use super::tool_display::{
    RenderCtx, ToolLines, append_annotation, append_right_info, assistant_style,
    build_instructions_lines, build_tool_lines, done_style, error_style, format_timestamp_now,
    thinking_style, truncate_to_header, user_style,
};
use super::{
    DisplayMessage, DisplayRole, DisplaySource, ToolRole, ToolStatus, apply_scroll_delta,
    code_view::SectionFlags, review,
};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{
    DiagramSpan, LinkMap, TerminalLink, hr_line, plain_lines, text_to_painted, truncate_output,
};
use crate::provenance::Provenance;
use crate::render_worker::RenderWorker;
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use crate::update;
use maki_config::{ClockFormat, ToolOutputLines, UiConfig};

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::time::Instant;

use super::scrollbar::render_vertical_scrollbar;
use super::streaming_content::StreamingContent;
use maki_agent::{
    BufferSnapshot, EventSender, InstructionBlock, NO_FILES_FOUND, SharedBuf, ToolDoneEvent,
    ToolOutput, ToolStartEvent,
};
use maki_lua::{EventHandle, WARM_TOOL_CAP, WinView};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::repaint::{Cadence, Dirty};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tracing::warn;

const THINKING_HIDDEN_HEADER: &str = "thinking> ...";
const REFLOW_MARGIN_VIEWPORTS: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
enum HoverTarget {
    Link(Arc<str>),
    CachedThinking(usize),
    StreamingThinking,
    Tool { id: String, feedback: HoverFeedback },
    Diagram(DiagramKey),
}

/// Identifies one drawn diagram across rebuilds. The message index is the
/// same backlink segments already carry, and `id` counts diagrams within
/// that message, so both survive a reflow.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DiagramKey {
    msg_index: usize,
    id: u16,
}

/// A rendered segment handed to the review modal. `provenance` is absent for
/// segments with no markdown behind them, such as tool buffers.
pub(crate) struct ReviewTarget {
    pub lines: Vec<Line<'static>>,
    pub provenance: Option<Provenance>,
    pub label: &'static str,
}

/// The default review surface. Notes carrying it compile without a surface
/// attribute, so the constant is shared with the review modal.
pub(crate) const ASSISTANT_LABEL: &str = "assistant reply";

fn review_label(source: DisplaySource) -> &'static str {
    match source {
        DisplaySource::User(_) => "your message",
        DisplaySource::AssistantText(_) => ASSISTANT_LABEL,
        DisplaySource::Reasoning(_) => "thinking",
        DisplaySource::ToolCall { .. } => "tool call",
        DisplaySource::ToolResult(_) => "tool result",
    }
}

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll_top: u16,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    viewport_area: Rect,
    cache: SegmentCache,
    last_total_lines: u16,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    /// Horizontal offset per drawn diagram. Absent means unpanned, so the
    /// map stays empty for the overwhelming majority of transcripts.
    diagram_pans: HashMap<DiagramKey, u16>,
    /// Per-tool log of post-completion click rows, replayed on restore.
    lua_clicks: HashMap<String, Vec<usize>>,
    live_bufs: HashMap<String, Arc<SharedBuf>>,
    /// Bufs of finished tools we keep polling so runtime-side warm
    /// clicks stay visible. Purely local: every finished-tool click
    /// carries a restore fallback, so we never track the runtime's
    /// warm cache.
    watched_bufs: VecDeque<(String, Arc<SharedBuf>)>,
    tool_output_lines: ToolOutputLines,
    lua_event_handle: EventHandle,
    restore_event_tx: Option<EventSender>,
    show_thinking: bool,
    thinking_collapsed: bool,
    clock_format: ClockFormat,
    /// One re-bake per tool per generation; `snapshot_theme_gen`
    /// only bumps when colors actually land.
    rebake_requested: HashMap<String, u64>,
    prompt_progress: Option<PromptProgress>,
    hover: Option<HoverTarget>,
    terminal_links: Vec<TerminalLink>,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig, lua_event_handle: EventHandle) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll_top: u16::MAX,
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            viewport_area: Rect::default(),
            cache: SegmentCache::new(),
            last_total_lines: 0,
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            diagram_pans: HashMap::new(),
            lua_clicks: HashMap::new(),
            live_bufs: HashMap::new(),
            watched_bufs: VecDeque::new(),
            tool_output_lines: ui_config.tool_output_lines,
            lua_event_handle,
            restore_event_tx: None,
            show_thinking: ui_config.show_thinking,
            thinking_collapsed: !ui_config.show_thinking,
            clock_format: ui_config.clock_format,
            rebake_requested: HashMap::new(),
            prompt_progress: None,
            hover: None,
            terminal_links: Vec::new(),
        }
    }

    pub fn set_restore_channel(&mut self, event_tx: Option<EventSender>) {
        self.restore_event_tx = event_tx;
    }

    /// Hands back the index of the message, which [`Self::replace`] needs to
    /// correct it later.
    pub fn push(&mut self, msg: DisplayMessage) -> usize {
        self.messages.push(msg);
        self.messages.len() - 1
    }

    /// Drops the whole segment cache, so keep it for one-off corrections and
    /// never for streaming. Marking the message stale is not enough: only the
    /// segments the viewport reaches get reflowed, so a fix above it would
    /// keep painting the old bubble.
    pub fn replace(&mut self, index: usize, msg: DisplayMessage) {
        let Some(slot) = self.messages.get_mut(index) else {
            return;
        };
        *slot = msg;
        self.cache.clear();
    }

    pub fn remove(&mut self, index: usize) {
        if index >= self.messages.len() {
            return;
        }
        self.messages.remove(index);
        self.cache.clear();
    }

    pub fn load_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        if !self.show_thinking {
            for msg in &mut msgs {
                if matches!(msg.role, DisplayRole::Thinking) {
                    msg.thinking_collapsed = true;
                }
            }
        }
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.lua_clicks.clear();
        self.live_bufs.clear();
        self.watched_bufs.clear();
        self.rebake_requested.clear();
        self.highlight_segment = None;
        self.thinking_collapsed = !self.show_thinking;
    }

    pub fn bind_sources(&mut self, source_messages: &[DisplayMessage]) {
        let mut start = 0;
        for message in &mut self.messages {
            let Some(offset) = source_messages[start..]
                .iter()
                .position(|source| same_display_item(message, source))
            else {
                continue;
            };
            let source = &source_messages[start + offset];
            message.source = source.source;
            start += offset + 1;
            if start == source_messages.len() {
                break;
            }
        }
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.clear_hover();
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        self.clear_hover();
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_raw_input = event.raw_input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.annotation = event.annotation;
            msg.render_header = event.render_header;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_raw_input = event.raw_input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.annotation = event.annotation;
        msg.render_header = event.render_header;
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or("");
        truncate_to_header(&mut msg.text);
        let truncated = truncate_output(content, self.tool_output_lines.get(tool_name));
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        let had_live_buf = self.retire_live_buf(&event.id);
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        truncate_to_header(&mut msg.text);
        let done_annotation = event
            .annotation
            .as_deref()
            .map(str::to_owned)
            .or_else(|| event.output.annotation());
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }

        match &event.output {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text)
                if msg.render_snapshot.is_none() =>
            {
                if had_live_buf {
                    // The plugin streamed a body buf but no snapshot ever
                    // landed: this is the raw llm_output glitch users report.
                    warn!(
                        tool_id = %event.id,
                        tool = %event.tool,
                        is_error = event.is_error,
                        "live buf had no snapshot at tool_done; falling back to llm_output"
                    );
                }
                let tr = truncate_output(&text.text, self.tool_output_lines.get(&event.tool));
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            _ => {}
        }
        msg.tool_output = Some(Arc::new(event.output));
        msg.live_output = None;
        self.rebuild_tool_segment(&event.id);
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(tool_id, |msg| msg.text = summary.to_owned());
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.update_tool(tool_id, |msg| append_annotation(&mut msg.annotation, model));
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, false, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, true, theme_gen);
    }

    /// A subagent stamps its own cumulative usage on the task header, and that
    /// header is usually the last tool of the turn, so an existing stamp wins.
    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let last_tool = self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) => Some((tool.id.clone(), msg.turn_usage.is_none())),
            _ => None,
        });
        if let Some((id, unstamped)) = last_tool
            && unstamped
        {
            self.set_tool_turn_usage(&id, usage);
        }
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.update_tool(tool_id, |msg| msg.turn_usage = Some(usage));
    }

    fn upsert_instruction_segment(
        &mut self,
        parent_id: &str,
        blocks: &[InstructionBlock],
        parent_idx: usize,
    ) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let exp = self
            .expanded_tools
            .get(&inst_id)
            .copied()
            .unwrap_or_default();
        let width = SegmentChrome::for_kind(SegmentKind::Instruction, self.viewport_width, 0)
            .content_width(self.viewport_width);
        let tl = build_instructions_lines(blocks, width, exp.output);

        if let Some(seg_idx) = self.cache.find_by_tool_id(&inst_id) {
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = tl.search_text.clone();
            seg.update_with_reuse(tl, &self.hl_worker);
        } else {
            let mut seg = Segment::with_tool(inst_id, SegmentKind::Instruction);
            seg.search_text = tl.search_text.clone();
            seg.apply_highlight(tl, &self.hl_worker);
            self.cache.insert(parent_idx + 1, seg);
        }
        self.cache.update_margins(self.viewport_width);
    }

    fn update_tool(&mut self, tool_id: &str, update_msg: impl FnOnce(&mut DisplayMessage)) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        update_msg(msg);
        self.rebuild_tool_segment(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.thinking_collapsed = !self.show_thinking;
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.fail_in_progress_except(message, &HashSet::new());
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                    && !excluded.contains(&t.id)
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                id,
                tool,
                output: ToolOutput::Plain(message.clone().into()),
                is_error: true,
                annotation: None,
                written_path: None,
                written_paths: Vec::new(),
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            // The stale-run_id filter drops these tools' ToolDone events,
            // so retire their live bufs here: keeps them clickable via
            // the warm path and stops them being polled forever.
            self.retire_live_buf(id);
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        entry.script = !entry.script;
        entry.output = !entry.output;
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn rebake_requested_gen(&self, tool_id: &str) -> Option<u64> {
        self.rebake_requested.get(tool_id).copied()
    }

    #[cfg(test)]
    pub fn snapshot_gen_of(&self, tool_id: &str) -> Option<u64> {
        self.current_snapshot_gen(tool_id)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) if tool.id == tool_id => msg.turn_usage.as_deref(),
            _ => None,
        })
    }

    pub fn set_prompt_progress(&mut self, progress: Option<PromptProgress>) {
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.set_scroll_top(apply_scroll_delta(self.scroll_top, delta));
    }

    /// Always unpins, and the next `view` re-pins if this lands on the
    /// bottom line.
    pub fn set_scroll_top(&mut self, top: u16) {
        self.clear_hover();
        self.scroll_top = top.min(self.max_scroll());
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.set_scroll_top(0);
    }

    pub fn enable_auto_scroll(&mut self) {
        self.clear_hover();
        self.auto_scroll = true;
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        let width = self.viewport_width;
        let offset = self
            .cache
            .segments()
            .iter()
            .take(segment_index)
            .map(|s| s.height(width) as u32)
            .sum::<u32>()
            .min(u16::MAX as u32) as u16;
        self.set_scroll_top(offset);
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.clear_hover();
        self.scroll_top = scroll_top;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn tool_id_at(&self, row: u16, area: Rect) -> Option<&str> {
        if area.height == 0 {
            return None;
        }
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        (rel >= segment.chrome(self.viewport_width).margin_top)
            .then_some(segment.tool_id.as_deref())
            .flatten()
    }

    pub fn source_at(&self, row: u16, area: Rect) -> Option<DisplaySource> {
        if area.height == 0 || row < area.y || row >= area.bottom() {
            return None;
        }
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(self.viewport_width).margin_top {
            return None;
        }
        self.segment_source(segment)
    }

    fn segment_source(&self, segment: &Segment) -> Option<DisplaySource> {
        if let Some(index) = segment.msg_index {
            return self.messages.get(index)?.source;
        }
        let tool_id = segment
            .tool_id
            .as_deref()
            .and_then(segment::instruction_parent)
            .or(segment.tool_id.as_deref())?;
        self.messages.iter().rev().find_map(|message| {
            matches!(&message.role, DisplayRole::Tool(tool) if tool.id == tool_id)
                .then_some(message.source)
                .flatten()
        })
    }

    pub(crate) fn update_hover(&mut self, row: u16, col: u16, area: Rect, known_task_target: bool) {
        self.hover = self.hover_target_at(row, col, area, known_task_target);
    }

    pub(crate) fn clear_hover(&mut self) {
        self.hover = None;
    }

    pub(crate) fn hovered_link(&self) -> Option<&str> {
        match &self.hover {
            Some(HoverTarget::Link(target)) => Some(target),
            _ => None,
        }
    }

    pub(crate) fn terminal_links(&self) -> &[TerminalLink] {
        &self.terminal_links
    }

    pub(crate) fn link_at(&self, row: u16, col: u16, area: Rect) -> Option<Arc<str>> {
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
        let rel_col = col - area.x;
        if let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) {
            let rel_row = u16::try_from(doc_row - start).ok()?;
            return segment.link_at(rel_row, rel_col, width);
        }
        self.streaming_link_at(doc_row, rel_col, width)
    }

    fn hover_target_at(
        &self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
    ) -> Option<HoverTarget> {
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
        let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) else {
            if let Some(target) = self.streaming_link_at(doc_row, col - area.x, width) {
                return Some(HoverTarget::Link(target));
            }
            return self
                .is_collapsed_streaming_thinking_row(doc_row, width)
                .then_some(HoverTarget::StreamingThinking);
        };
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        if let Some(target) = segment.link_at(rel, col - area.x, width) {
            return Some(HoverTarget::Link(target));
        }
        let Some(tool_id) = segment.tool_id.as_deref() else {
            let msg_index = segment.msg_index?;
            // A diagram row wins over the thinking toggle: it is the only
            // target inside a text segment that reacts to the pointer.
            if let Some(key) = self.diagram_key_at(segment, msg_index, rel, width) {
                return Some(HoverTarget::Diagram(key));
            }
            return self
                .messages
                .get(msg_index)
                .is_some_and(|message| {
                    matches!(message.role, DisplayRole::Thinking) && message.thinking_collapsed
                })
                .then_some(HoverTarget::CachedThinking(msg_index));
        };

        let expanded = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let native_toggle =
            !self.has_snapshot(tool_id) && (segment.truncation.any() || expanded.any());
        if !native_toggle && !known_task_target {
            return None;
        }
        let feedback = if native_toggle
            && segment.lines().iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains(EXPAND_AFFORDANCE))
            }) {
            HoverFeedback::Affordance
        } else {
            HoverFeedback::Chrome
        };
        Some(HoverTarget::Tool {
            id: tool_id.to_owned(),
            feedback,
        })
    }

    fn hover_feedback_for_segment(&self, segment: &Segment) -> Option<HoverFeedback> {
        match &self.hover {
            Some(HoverTarget::CachedThinking(msg_index))
                if segment.msg_index == Some(*msg_index) && segment.tool_id.is_none() =>
            {
                Some(HoverFeedback::Affordance)
            }
            Some(HoverTarget::Tool { id, feedback })
                if segment.tool_id.as_deref() == Some(id.as_str()) =>
            {
                Some(*feedback)
            }
            Some(HoverTarget::CachedThinking(_))
            | Some(HoverTarget::Link(_))
            | Some(HoverTarget::StreamingThinking)
            | Some(HoverTarget::Tool { .. })
            | Some(HoverTarget::Diagram(_))
            | None => None,
        }
    }

    fn streaming_link_at(&self, doc_row: u32, col: u16, width: u16) -> Option<Arc<str>> {
        let mut block_start = self.cache.total_height(width);
        let mut has_previous = self.cache.len() != 0;
        let streams = [
            (
                &self.streaming_thinking,
                self.streaming_thinking_collapsed(),
                SegmentKind::Thinking,
            ),
            (&self.streaming_text, false, SegmentKind::Assistant),
        ];
        for (stream, collapsed, kind) in streams {
            if stream.is_empty() {
                continue;
            }
            if has_previous {
                block_start = block_start.saturating_add(1);
            }
            let chrome = SegmentChrome::for_kind(kind, width, 0);
            let content_width = chrome.content_width(width);
            let lines = if collapsed {
                None
            } else {
                Some(stream.cached_lines())
            };
            let height = lines.map_or_else(
                || wrapped_line_count(&self.build_streaming_collapsed_lines(), content_width),
                |lines| wrapped_line_count(lines, content_width),
            ) as u32;
            if !collapsed && (block_start..block_start + height).contains(&doc_row) {
                let row = u16::try_from(doc_row - block_start).ok()?;
                let col = col.checked_sub(chrome.left)?;
                return stream.link_at(content_width, row, col);
            }
            block_start = block_start.saturating_add(height);
            has_previous = true;
        }
        None
    }

    /// The diagram drawn on `rel`, a row relative to the segment. Diagram
    /// rows are pre-sliced to the viewport, so the display row maps straight
    /// onto a line index.
    fn diagram_key_at(
        &self,
        segment: &Segment,
        msg_index: usize,
        rel: u16,
        width: u16,
    ) -> Option<DiagramKey> {
        if segment.diagrams().is_empty() {
            return None;
        }
        let line = segment.source_line_at(rel, width)?;
        segment.diagram_at_line(line).map(|span| DiagramKey {
            msg_index,
            id: span.id,
        })
    }

    /// The diagram the keyboard pans: whichever shows the most rows, and the
    /// later message when two show the same. Rows resolve through the lookup
    /// the pointer uses, because a line index is not a display row once
    /// anything above the diagram wraps.
    fn most_visible_diagram(&self) -> Option<DiagramKey> {
        let width = self.viewport_width;
        let mut rows: Vec<(DiagramKey, u16)> = Vec::new();
        for offset in 0..self.viewport_height {
            let doc_row = self.scroll_top as u32 + offset as u32;
            let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) else {
                continue;
            };
            let Some(key) = segment
                .msg_index
                .zip(u16::try_from(doc_row - start).ok())
                .and_then(|(msg_index, rel)| self.diagram_key_at(segment, msg_index, rel, width))
            else {
                continue;
            };
            match rows.iter_mut().find(|(seen, _)| *seen == key) {
                Some((_, count)) => *count += 1,
                None => rows.push((key, 1)),
            }
        }
        // A chart that already fits has nothing to show for a keypress, and
        // targeting it would strand the keys on a wide chart further up.
        rows.retain(|(key, _)| self.pan_range(*key).is_some_and(|(_, max)| max > 0));
        // Rows are collected top down, so taking the last of the equal maxima
        // is what breaks a tie toward the later message.
        rows.into_iter()
            .reduce(|best, next| if next.1 >= best.1 { next } else { best })
            .map(|(key, _)| key)
    }

    /// The segment holding a chart and how far it may travel. The ceiling is
    /// zero for a chart that already fits, which is what makes it a poor
    /// target for a key that has no pointer to disambiguate it.
    fn pan_range(&self, key: DiagramKey) -> Option<(usize, u16)> {
        let width = self.viewport_width;
        let (seg_idx, full_width) =
            self.cache
                .segments()
                .iter()
                .enumerate()
                .find_map(|(idx, segment)| {
                    if segment.msg_index != Some(key.msg_index) {
                        return None;
                    }
                    let span = segment.diagrams().iter().find(|span| span.id == key.id)?;
                    Some((idx, span.full_width))
                })?;
        let content = self
            .cache
            .get(seg_idx)
            .map_or(width, |segment| segment.content_width(width));
        Some((
            seg_idx,
            maki_markdown::render::diagram_max_pan(full_width, content),
        ))
    }

    /// Scrolls a drawn diagram sideways, reporting whether anything moved.
    fn pan_diagram(&mut self, key: DiagramKey, delta: i32) -> bool {
        let width = self.viewport_width;
        let Some((seg_idx, ceiling)) = self.pan_range(key) else {
            return false;
        };

        let current = self.diagram_pans.get(&key).copied().unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, ceiling as i32) as u16;
        if next == current {
            return false;
        }
        match next {
            0 => self.diagram_pans.remove(&key),
            _ => self.diagram_pans.insert(key, next),
        };
        self.reflow_text_segment(seg_idx, width);
        self.cache.update_margins(width);
        true
    }

    /// Pans the diagram under the pointer. Used by horizontal wheel events.
    pub(crate) fn pan_hovered_diagram(&mut self, delta: i32) -> bool {
        let Some(HoverTarget::Diagram(key)) = self.hover else {
            return false;
        };
        self.pan_diagram(key, delta)
    }

    /// Pans without a pointer, for the keyboard bindings.
    pub(crate) fn pan_visible_diagram(&mut self, delta: i32) -> bool {
        let Some(key) = self.most_visible_diagram() else {
            return false;
        };
        self.pan_diagram(key, delta)
    }

    #[cfg(test)]
    pub(crate) fn panned_diagram_count(&self) -> usize {
        self.diagram_pans.len()
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        self.clear_hover();
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let width = self.viewport_width;
        // Both fallbacks toggle thinking: a row past the cached segments
        // belongs to the still-streaming indicator, and a segment without a
        // tool_id is a finished message's text.
        let Some((_, seg, seg_start)) = self.cache.segment_at_row(doc_row, width) else {
            return self.try_toggle_collapsed_thinking(doc_row, width);
        };
        let rel = u16::try_from(doc_row - seg_start).unwrap_or(u16::MAX);
        if rel < seg.chrome(width).margin_top {
            return false;
        }
        let Some(tool_id) = seg.tool_id.as_deref() else {
            let msg_idx = seg.msg_index;
            return self.try_toggle_cached_thinking(msg_idx, width);
        };

        if self.has_snapshot(tool_id) {
            let buf_row = seg.source_line_at(rel, width).map_or(0, |l| seg.buf_row(l));
            if self.tool_in_progress(tool_id) {
                self.lua_event_handle
                    .request_click(tool_id.to_owned(), buf_row);
                return true;
            }
            // Recorded even when the warm path serves the click: theme
            // rebake and session restore replay the full sequence.
            self.lua_clicks
                .entry(tool_id.to_owned())
                .or_default()
                .push(buf_row);
            let item = self.lua_restore_item(tool_id).map(|mut item| {
                item.clicks = self.lua_clicks[tool_id].clone();
                item
            });
            let Some(tx) = self.restore_event_tx.clone() else {
                return true;
            };
            let eh = &self.lua_event_handle;
            // Watching the buf means a runtime-side warm click would be
            // visible here, so try the fast path; the fallback item lets
            // the runtime degrade to restore+replay if its cache is cold.
            // Without the buf only a fresh restore can show the result.
            match (self.watching(tool_id), item) {
                (true, Some(item)) => {
                    eh.request_click_with_fallback(tool_id.to_owned(), buf_row, item, tx);
                }
                (true, None) => eh.request_click(tool_id.to_owned(), buf_row),
                (false, Some(item)) => eh.request_restore(item, tx),
                (false, None) => {}
            }
            return true;
        }

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let truncation = seg.truncation;

        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if truncation.output || entry.output {
            entry.output = !entry.output;
        } else if truncation.script || entry.script {
            entry.script = !entry.script;
        }
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        self.handle_click(row, area)
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(parent_idx) = self.cache.find_by_tool_id(parent_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks, parent_idx);
            }
        } else {
            self.rebuild_tool_segment(tool_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        msg.tool_output.as_deref()?.owned_instructions()
    }

    /// Drains the highlight worker and every live tool buffer. These used to
    /// run inside [`Self::view`], which is why a running tool had to claim it
    /// was animating: it was the only way to keep them fed.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = self.drain_highlights() | self.poll_live_bufs();
        if self.show_idle_splash() {
            dirty |= self.idle_splash.poll_update(update::latest_version());
        }
        dirty
    }

    pub fn cadence(&self) -> Cadence {
        // Collapsed thinking draws a line count, not the text, so its
        // typewriter reveals nothing and never advances either, since only
        // `view` ticks it. Believing it would pin the loop at full frame rate
        // for the whole reasoning phase.
        let smooth = self.streaming_text.is_animating()
            || self.accent.is_animating()
            || (self.streaming_thinking.is_animating() && !self.streaming_thinking_collapsed());
        Cadence::any([
            // A running tool draws a spinner. Its output arriving is data, and
            // `tick` reports that separately.
            Cadence::when(self.in_progress_count() > 0, Cadence::SPINNER),
            Cadence::when(smooth, Cadence::SMOOTH),
            Cadence::when(self.show_idle_splash(), self.idle_splash.cadence()),
        ])
    }

    fn streaming_thinking_collapsed(&self) -> bool {
        self.thinking_collapsed && !self.streaming_thinking.is_empty()
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.terminal_links.clear();
        if self.viewport_area != area {
            self.clear_hover();
            self.viewport_area = area;
        }
        let previous_scroll_top = self.scroll_top;
        let previous_total_lines = self.last_total_lines;
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let theme_changed = self.theme_generation != theme_gen;
        if self.viewport_width != width {
            self.clear_hover();
        }
        let width_changed = self.viewport_width != width || theme_changed;
        if width_changed {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
        }
        if theme_changed {
            self.rebake_stale_snapshots(theme_gen);
        }

        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            self.idle_splash.render(area, frame.buffer_mut(), accent);
            return;
        }

        if width_changed {
            self.cache.mark_all_width_stale();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
            );
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 {
            self.update_spinners();
        }

        let cached_count = self.cache.len();
        let spacer_lines: [Line<'static>; 1] = [Line::default()];
        let mut streaming_heights: Vec<u16> = Vec::new();

        let thinking_collapsed = self.streaming_thinking_collapsed();
        let collapsed_thinking_lines = if thinking_collapsed {
            self.build_streaming_collapsed_lines()
        } else {
            Vec::new()
        };

        if thinking_collapsed {
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
            streaming_heights.push(wrapped_line_count(&collapsed_thinking_lines, content_width));
        } else if !self.streaming_thinking.is_empty() {
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
            if self.streaming_thinking.update_render(content_width) {
                self.clear_hover();
            }
            let lines = self.streaming_thinking.cached_lines();
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(lines, content_width));
        }

        if !self.streaming_text.is_empty() {
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Assistant, width, 0).content_width(width);
            if self.streaming_text.update_render(content_width) {
                self.clear_hover();
            }
            let lines = self.streaming_text.cached_lines();
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(lines, content_width));
        }

        let streaming_sum: u32 = streaming_heights.iter().map(|&h| h as u32).sum();
        // The reflow window is picked from `scroll_top` and the bottom pin,
        // and the reflow changes the heights both are derived from: resolve
        // before to aim the window, and after to place the result.
        self.cache.update_margins(width);
        self.resolve_scroll(width, streaming_sum, has_selection);
        self.reflow_viewport(width, has_selection);
        self.cache.update_margins(width);
        let total_lines = self.resolve_scroll(width, streaming_sum, has_selection);
        if self.scroll_top != previous_scroll_top || total_lines != previous_total_lines {
            self.clear_hover();
        }

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(
            self.scroll_top,
            viewport,
            mem::take(&mut self.terminal_links),
        );

        let accent = self.accent.resolve();
        for (i, seg) in self.cache.segments().iter().enumerate() {
            if cursor.past_bottom() {
                break;
            }
            let h = seg.height(width);
            let highlight = self.highlight_segment == Some(i);
            let hover = self
                .hover_feedback_for_segment(seg)
                .map(|feedback| (feedback, accent));
            cursor.render(
                (seg.lines(), Some(seg.links())),
                h,
                seg.chrome(width),
                segment_styles(seg.kind(), accent),
                RenderFeedback { highlight, hover },
                frame,
            );
        }

        let mut height_idx = 0usize;
        let streamed: [(&StreamingContent, bool, SegmentKind); 2] = [
            (
                &self.streaming_thinking,
                thinking_collapsed,
                SegmentKind::Thinking,
            ),
            (&self.streaming_text, false, SegmentKind::Assistant),
        ];
        for (sc, collapsed, kind) in streamed {
            if sc.is_empty() || height_idx >= streaming_heights.len() || cursor.past_bottom() {
                continue;
            }
            if cached_count > 0 || height_idx > 0 {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                cursor.render(
                    (&spacer_lines, None),
                    h,
                    SegmentChrome::for_kind(SegmentKind::Assistant, width, 0),
                    (None, None),
                    RenderFeedback::default(),
                    frame,
                );
            }
            if height_idx < streaming_heights.len() {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                if collapsed {
                    let hover = matches!(self.hover, Some(HoverTarget::StreamingThinking))
                        .then_some((HoverFeedback::Affordance, accent));
                    cursor.render(
                        (&collapsed_thinking_lines, None),
                        h,
                        SegmentChrome::for_kind(kind, width, 0),
                        (None, None),
                        RenderFeedback {
                            hover,
                            ..RenderFeedback::default()
                        },
                        frame,
                    );
                } else {
                    cursor.render(
                        (sc.cached_lines(), Some(sc.links())),
                        h,
                        SegmentChrome::for_kind(kind, width, 0),
                        (None, None),
                        RenderFeedback::default(),
                        frame,
                    );
                }
            }
        }
        self.terminal_links = cursor.into_terminal_links();

        if let Some(pp) = self.prompt_progress
            && pp.total > 0
        {
            let ratio = pp.processed as f64 / pp.total as f64;
            let bar_width = (width as f64 * 0.1).round() as u16;
            let label = " Processing ";
            let label_width = label.len() as u16;
            let total_width = label_width + bar_width;
            let bar_x = area.x + width.saturating_sub(total_width);
            let bar_y = area.y + area.height.saturating_sub(1);
            let bar_area = Rect::new(bar_x, bar_y, total_width, 1);
            crate::components::progress_bar::render(
                frame,
                bar_area,
                &crate::components::progress_bar::ProgressBarConfig {
                    ratio,
                    style: theme::current().progress_bar,
                    cache_ratio: pp.cache as f64 / pp.total as f64,
                    cache_style: Style::new().fg(Color::Green),
                    label: Some(label),
                    label_style: Some(theme::current().tool_dim),
                    bar_width,
                },
            );
            self.terminal_links
                .retain(|link| !bar_area.contains(link.position));
        }

        if total_lines > area.height {
            render_vertical_scrollbar(frame, area, total_lines, self.scroll_top);
        }
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines.saturating_sub(self.viewport_height)
    }

    pub fn scroll_top(&self) -> u16 {
        self.scroll_top
    }

    /// Backs `maki.fn.winsaveview`. The clamp matters: a pinned or restored
    /// `scroll_top` can sit past the end until the next `view` resolves it
    /// against the current line count.
    pub fn win_view(&self) -> WinView {
        WinView {
            scroll_top: self.scroll_top.min(self.max_scroll()),
            line_count: self.last_total_lines,
            height: self.viewport_height,
            auto_scroll: self.auto_scroll,
        }
    }

    /// Raw markdown of the newest assistant reply. Streaming text has not
    /// been committed yet, so it is preferred when present.
    pub fn last_reply_source(&self) -> Option<String> {
        if !self.streaming_text.is_empty() {
            return Some(self.streaming_text.buffer().to_owned());
        }
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, DisplayRole::Assistant))
            .map(|m| m.text.clone())
    }

    /// Newest committed assistant reply, the default target for a review.
    /// Streaming text has no segment yet, so it cannot be reviewed.
    pub fn last_assistant_source(&self) -> Option<DisplaySource> {
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, DisplayRole::Assistant))
            .and_then(|m| m.source)
    }

    /// Painted lines and provenance for the segment behind `source`, so the
    /// review modal can re-lay them at its own width and quote the markdown
    /// that produced them.
    pub(crate) fn review_target(&self, source: DisplaySource) -> Option<ReviewTarget> {
        let segment = self
            .cache
            .segments()
            .iter()
            .find(|segment| self.segment_source(segment) == Some(source))?;
        if segment.lines().is_empty() {
            return None;
        }
        Some(ReviewTarget {
            lines: segment.lines().to_vec(),
            provenance: segment.provenance().cloned(),
            label: review_label(source),
        })
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        let width = self.viewport_width;
        self.cache
            .segments()
            .iter()
            .map(|s| s.height(width))
            .collect()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.cache.search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn tool_in_progress(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rev()
            .find_map(|m| match &m.role {
                DisplayRole::Tool(t) if t.id == tool_id => Some(t.status),
                _ => None,
            })
            .is_some_and(|s| s == ToolStatus::InProgress)
    }

    fn watching(&self, tool_id: &str) -> bool {
        self.watched_bufs.iter().any(|(id, _)| id == tool_id)
    }

    fn stop_watching(&mut self, tool_id: &str) {
        self.watched_bufs.retain(|(id, _)| id != tool_id);
    }

    /// Moves a finished tool's live buf to the watched set, flushing any
    /// last dirty lines. Called on completion and on cancellation, so
    /// `live_bufs` never leaks entries that outlive their tool, and the capped
    /// watched set keeps what `tick` polls bounded.
    /// Returns whether a live buf existed for this id.
    fn retire_live_buf(&mut self, id: &str) -> bool {
        let Some(buf) = self.live_bufs.remove(id) else {
            return false;
        };
        if let Some(lines) = buf.read_if_dirty() {
            self.store_snapshot(id, BufferSnapshot::from_arc(lines), false, None);
        }
        self.watched_bufs.push_back((id.to_owned(), buf));
        if self.watched_bufs.len() > WARM_TOOL_CAP {
            self.watched_bufs.pop_front();
        }
        true
    }

    fn has_snapshot(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .is_some_and(|m| m.render_snapshot.is_some())
    }

    fn lua_restore_item(&self, tool_id: &str) -> Option<maki_lua::RestoreItem> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        crate::chat::restore_item_for(msg, self.tool_output_lines, self.theme_generation)
    }

    /// Re-restores every snapshot still painted with old-theme colors.
    /// Replies carry a generation so stale ones can't overwrite fresher colors.
    fn rebake_stale_snapshots(&mut self, current_gen: u64) {
        let Some(tx) = self.restore_event_tx.clone() else {
            return;
        };
        let eh = &self.lua_event_handle;
        self.rebake_requested.retain(|_, g| *g >= current_gen);
        let tol = self.tool_output_lines;
        let mut requested = Vec::new();
        for msg in &self.messages {
            let DisplayRole::Tool(role) = &msg.role else {
                continue;
            };
            if !self.should_request_rebake(
                &role.id,
                msg.snapshot_is_stale(current_gen),
                current_gen,
            ) {
                continue;
            }
            if let Some(mut item) = crate::chat::restore_item_for(msg, tol, current_gen) {
                item.clicks = self.lua_clicks.get(&role.id).cloned().unwrap_or_default();
                eh.request_restore(item, tx.clone());
                requested.push(role.id.clone());
            }
        }
        for id in requested {
            // The watched buf still carries old-theme lines; clicks in
            // the rebake window must go through restore, not warm.
            self.stop_watching(&id);
            self.rebake_requested.insert(id, current_gen);
        }
    }

    fn should_request_rebake(&self, tool_id: &str, stale: bool, current_gen: u64) -> bool {
        stale && self.rebake_requested.get(tool_id) != Some(&current_gen)
    }

    /// Live snapshots (`None`) get the panel's current generation.
    /// Re-bake replies are monotonic: drop if something newer landed.
    fn resolve_snapshot_gen(&self, tool_id: &str, incoming: Option<u64>) -> Option<u64> {
        let Some(incoming_gen) = incoming else {
            return Some(self.theme_generation);
        };
        match self.current_snapshot_gen(tool_id) {
            Some(applied) if applied > incoming_gen => None,
            _ => Some(incoming_gen),
        }
    }

    fn current_snapshot_gen(&self, tool_id: &str) -> Option<u64> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .map(|m| m.snapshot_theme_gen)
    }

    fn store_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        is_header: bool,
        theme_gen: Option<u64>,
    ) {
        if theme_gen.is_some() {
            // A generation only comes with restore replies. The restore
            // superseded the old live view (and evicted the runtime's
            // warm handle), so its buf must not overwrite this snapshot.
            self.stop_watching(tool_id);
        }
        let Some(applied_gen) = self.resolve_snapshot_gen(tool_id, theme_gen) else {
            return;
        };
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            if is_header {
                msg.text = snapshot.first_line_text();
                msg.render_header = Some(snapshot);
            } else {
                msg.render_snapshot = Some(snapshot);
            }
            msg.snapshot_theme_gen = applied_gen;
            self.rebuild_tool_segment(tool_id);
        } else {
            warn!(
                tool_id,
                is_header, "snapshot dropped: no tool message with this id"
            );
        }
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            started_at: self.started_at,
            width: SegmentChrome::for_kind(SegmentKind::ToolBlock, self.viewport_width, 0)
                .content_width(self.viewport_width),
            tool_output_lines: &self.tool_output_lines,
        }
    }

    pub fn register_live_buf(&mut self, id: String, body: Arc<SharedBuf>) {
        self.live_bufs.insert(id, body);
    }

    /// Snapshots are baked at the last width `view` saw, and a resize
    /// invalidates every segment anyway (see `width_changed` in `view`), so
    /// polling ahead of the frame that reflows them is safe.
    fn poll_live_bufs(&mut self) -> Dirty {
        let updated: Vec<_> = self
            .live_bufs
            .iter()
            .chain(self.watched_bufs.iter().map(|(id, buf)| (id, buf)))
            .filter_map(|(id, buf)| buf.read_if_dirty().map(|lines| (id.clone(), lines)))
            .collect();
        let dirty = Dirty::from(!updated.is_empty());
        for (tool_id, lines) in updated {
            self.store_snapshot(&tool_id, BufferSnapshot::from_arc(lines), false, None);
        }
        dirty
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: SectionFlags,
    ) -> ToolLines {
        let mut tl = build_tool_lines(msg, status, rctx, exp);
        if let Some(ts) = &msg.timestamp
            && !tl.lines.is_empty()
        {
            append_right_info(
                &mut tl.lines[0],
                msg.turn_usage.as_deref(),
                Some(ts),
                rctx.width,
            );
            tl.links.rows[0] = vec![None; tl.lines[0].spans.len()];
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.thinking_collapsed = self.thinking_collapsed;
        self.thinking_collapsed = !self.show_thinking;
        self.messages.push(msg);
    }

    fn build_streaming_collapsed_lines(&self) -> Vec<Line<'static>> {
        thinking_indicator(self.streaming_thinking.line_count())
    }

    fn build_cached_thinking_indicator(&self, text: &str) -> Vec<Line<'static>> {
        thinking_indicator(logical_line_count(text))
    }

    fn try_toggle_collapsed_thinking(&mut self, doc_row: u32, width: u16) -> bool {
        if !self.is_collapsed_streaming_thinking_row(doc_row, width) {
            return false;
        }
        self.thinking_collapsed = false;
        true
    }

    fn is_collapsed_streaming_thinking_row(&self, doc_row: u32, width: u16) -> bool {
        if !self.streaming_thinking_collapsed() {
            return false;
        }
        let cached_height = self.cache.total_height(width);
        let spacer = if self.cache.len() > 0 { 1 } else { 0 };
        let thinking_start = cached_height + spacer;
        let content_width =
            SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
        let height =
            wrapped_line_count(&self.build_streaming_collapsed_lines(), content_width) as u32;
        doc_row >= thinking_start && doc_row < thinking_start + height
    }

    fn try_toggle_cached_thinking(&mut self, msg_idx: Option<usize>, width: u16) -> bool {
        if self.show_thinking {
            return false;
        }
        let Some(idx) = msg_idx else { return false };
        let Some(msg) = self.messages.get_mut(idx) else {
            return false;
        };
        if !matches!(msg.role, DisplayRole::Thinking) {
            return false;
        }
        msg.thinking_collapsed = !msg.thinking_collapsed;
        self.rebuild_thinking_segment(idx, width);
        true
    }

    fn rebuild_thinking_segment(&mut self, msg_idx: usize, width: u16) {
        let Some((text, collapsed)) = self
            .messages
            .get(msg_idx)
            .map(|m| (m.text.clone(), m.thinking_collapsed))
        else {
            return;
        };
        let (lines, links) = if collapsed {
            let lines = self.build_cached_thinking_indicator(&text);
            let links = LinkMap::none_for(&lines);
            (lines, links)
        } else {
            let style = thinking_style();
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
            let (painted, _) = text_to_painted(
                &text,
                style.prefix,
                style.text_style,
                style.prefix_style,
                content_width,
                None,
                Vec::new(),
            );
            (painted.lines, painted.links)
        };
        let search_text = format!("thinking> {text}");
        let seg_idx = self
            .cache
            .segments()
            .iter()
            .position(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none());
        let Some(seg_idx) = seg_idx else { return };
        if let Some(seg) = self.cache.get_mut(seg_idx) {
            seg.set_lines(lines);
            seg.set_links(links);
            seg.search_text = search_text;
        }
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            seg.update_spinners(&spinner_span);
        }
    }

    fn drain_highlights(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(result.lines);
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        self.clear_hover();
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            unreachable!()
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let rctx = self.rctx();
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp);

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());

        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.search_text = tl.search_text.clone();
        seg.update_with_reuse(tl, &self.hl_worker);

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx);
        }
        self.cache.update_margins(self.viewport_width);
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.expanded_tools.get(&t.id).copied().unwrap_or_default();
                let status = t.status;
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
                let id = t.id.clone();
                let search_text = tl.search_text.clone();
                let mut seg = Segment::with_tool(id.clone(), SegmentKind::ToolBlock);
                seg.search_text = search_text;
                seg.apply_highlight(tl, &self.hl_worker);
                self.cache.push(seg);

                let blocks = msg
                    .tool_output
                    .as_deref()
                    .and_then(|o| o.owned_instructions());
                if let Some(blocks) = blocks {
                    let last_idx = self.cache.len().saturating_sub(1);
                    self.upsert_instruction_segment(&id, &blocks, last_idx);
                }
            } else {
                if matches!(&msg.role, DisplayRole::Thinking) && msg.thinking_collapsed {
                    let text = msg.text.clone();
                    let lines = self.build_cached_thinking_indicator(&text);
                    let search_text = format!("thinking> {text}");
                    let mut segment = Segment::with_lines(lines, search_text, Some(i));
                    segment.set_links(LinkMap::none_for(segment.lines()));
                    segment.set_kind(SegmentKind::Thinking);
                    self.cache.push(segment);
                    continue;
                }
                let built = build_message_lines(msg, self.viewport_width, self.pans_for(i));
                let mut segment = Segment::with_lines(built.lines, built.search_text, Some(i));
                segment.set_kind(segment_kind(&msg.role));
                segment.set_provenance(built.provenance);
                segment.set_diagrams(built.diagrams);
                segment.set_links(built.links);
                self.cache.push(segment);
            }
        }
        self.cache.update_margins(self.viewport_width);
        self.cache.mark_built(self.messages.len());
    }

    /// Clamps `scroll_top` against the document height and applies the bottom
    /// pin, returning the total the scrollbar draws from.
    fn resolve_scroll(&mut self, width: u16, streaming_sum: u32, has_selection: bool) -> u16 {
        let total_lines: u16 =
            (self.cache.total_height(width) + streaming_sum).min(u16::MAX as u32) as u16;
        self.last_total_lines = total_lines;
        let max_scroll = total_lines.saturating_sub(self.viewport_height);
        self.scroll_top = self.scroll_top.min(max_scroll);
        if !has_selection {
            if self.scroll_top >= max_scroll {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll_top = max_scroll;
            }
        }
        total_lines
    }

    /// Re-lays out the stale segments the viewport plus its margin reaches,
    /// keeping the topmost visible one pinned: `scroll_top` is a line offset,
    /// so re-laying out anything above the viewport would slide the content
    /// the reader is looking at.
    ///
    /// The window is walked outward from an anchor segment, counting heights
    /// only after each segment is reflowed. Document offsets move as the
    /// reflow runs, so a window expressed in them would need repeated passes
    /// to settle; this one is right the first time.
    fn reflow_viewport(&mut self, width: u16, has_selection: bool) {
        // `resolve_scroll` only pins to the bottom when it owns the scroll, so
        // that is exactly when the viewport is the document tail.
        let pinned_to_bottom = self.auto_scroll && !has_selection;
        let anchor = (!pinned_to_bottom)
            .then(|| self.cache.anchor_at(self.scroll_top as u32, width))
            .flatten();
        let viewport = self.viewport_height as u32;
        let margin = viewport.saturating_mul(REFLOW_MARGIN_VIEWPORTS);
        let below = viewport.saturating_add(margin);
        // Pinned to the bottom (or scrolled into the streaming tail): the
        // viewport is the last `below` lines, so the window is all above.
        //
        // Anchored, the first visible row sits `rel` rows into the anchor
        // segment, so the downward window has to clear those before it starts
        // covering the viewport; counting from the segment's start instead
        // leaves stale segments on screen whenever `rel` exceeds the margin.
        let (start, above, below) = match anchor {
            Some((i, rel)) => (i, margin, below.saturating_add(rel as u32)),
            None => (self.cache.len().saturating_sub(1), below, below),
        };
        let len_before = self.cache.len();

        let mut acc = 0;
        let mut i = start;
        while i < self.cache.len() && acc < below {
            acc += self.reflowed_height(i, width);
            i += 1;
        }
        let mut acc = 0;
        let mut i = start;
        while i > 0 && acc < above {
            i -= 1;
            acc += self.reflowed_height(i, width);
        }

        // A rebuild can insert a missing instruction segment, which shifts the
        // anchor index. Rare enough to just skip the pin for one frame.
        if let Some(anchor) = anchor
            && self.cache.len() == len_before
        {
            self.scroll_top = self.cache.anchor_offset(anchor, width).min(u16::MAX as u32) as u16;
        }
    }

    /// Reflows `seg_idx` if it is stale, then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        // A tool segment and its instruction segment both map back to the
        // same parent, and one `rebuild_tool_segment` clears both flags.
        // Re-check so the parent is not rebuilt twice.
        if self.cache.get(seg_idx).is_some_and(|s| s.stale) {
            self.reflow_segment(seg_idx, width);
        }
        self.cache
            .get(seg_idx)
            .map_or(0, |s| s.height(width) as u32)
    }

    fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
        // Clear up front so a reflow that bails early (message gone, empty
        // instructions) costs one frame of old-width lines instead of
        // retrying forever.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.stale = false;
        let (tool_id, msg_idx) = (seg.tool_id.clone(), seg.msg_index);

        if let Some(tid) = tool_id {
            let parent = segment::instruction_parent(&tid)
                .map(str::to_string)
                .unwrap_or(tid);
            self.rebuild_tool_segment(&parent);
            return;
        }

        let Some(msg_idx) = msg_idx else {
            return;
        };

        let collapsed = self
            .messages
            .get(msg_idx)
            .is_some_and(|m| matches!(m.role, DisplayRole::Thinking) && m.thinking_collapsed);
        if collapsed {
            // Geometry is width-independent, but `width_changed` also fires on
            // theme changes; rebuild so spans pick up the new palette.
            self.rebuild_thinking_segment(msg_idx, width);
        } else {
            self.reflow_text_segment(seg_idx, width);
        }
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(msg) = self.messages.get(msg_idx) else {
            return;
        };
        let built = build_message_lines(msg, width, self.pans_for(msg_idx));
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.set_lines(built.lines);
        seg.set_provenance(built.provenance);
        seg.set_diagrams(built.diagrams);
        seg.set_links(built.links);
        seg.search_text = built.search_text;
    }

    /// Pans for one message, indexed by diagram id. Empty when nothing in
    /// the message has been panned, which is the usual case.
    fn pans_for(&self, msg_index: usize) -> Vec<u16> {
        let highest = self
            .diagram_pans
            .keys()
            .filter(|key| key.msg_index == msg_index)
            .map(|key| key.id)
            .max();
        let Some(highest) = highest else {
            return Vec::new();
        };
        (0..=highest)
            .map(|id| {
                self.diagram_pans
                    .get(&DiagramKey { msg_index, id })
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    }
}

fn same_display_item(left: &DisplayMessage, right: &DisplayMessage) -> bool {
    match (&left.role, &right.role) {
        (DisplayRole::Tool(left), DisplayRole::Tool(right)) => left.id == right.id,
        (DisplayRole::User, DisplayRole::User)
        | (DisplayRole::Assistant, DisplayRole::Assistant)
        | (DisplayRole::Thinking, DisplayRole::Thinking) => left.text == right.text,
        _ => false,
    }
}

/// Two-line thinking indicator: a header (`thinking> ...`) followed by a
/// `(N lines) (click to expand)` footer. Shared by the streaming and cached
/// views when `show_thinking` is off.
fn thinking_indicator(line_count: usize) -> Vec<Line<'static>> {
    let theme = theme::current();
    vec![
        Line::from(Span::styled(THINKING_HIDDEN_HEADER, theme.thinking)),
        Line::from(vec![
            Span::styled(format!("({line_count} lines) "), theme.tool_dim),
            Span::styled("(click to expand)", theme.thinking),
        ]),
    ]
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + 1
    }
}

fn segment_kind(role: &DisplayRole) -> SegmentKind {
    match role {
        DisplayRole::User => SegmentKind::User,
        DisplayRole::Assistant => SegmentKind::Assistant,
        DisplayRole::Thinking => SegmentKind::Thinking,
        DisplayRole::Error => SegmentKind::Error,
        DisplayRole::Done => SegmentKind::Done,
        DisplayRole::Tool(_) => SegmentKind::ToolBlock,
    }
}

fn segment_styles(kind: SegmentKind, accent: Color) -> (Option<Style>, Option<Style>) {
    let theme = theme::current();
    match kind {
        SegmentKind::User => (
            Some(theme.user_message_style()),
            Some(Style::new().fg(accent)),
        ),
        SegmentKind::ToolBlock | SegmentKind::Instruction => {
            (Some(theme.panel_style()), Some(theme.subtle_border_style()))
        }
        SegmentKind::Error => (Some(theme.panel_style()), Some(theme.error)),
        SegmentKind::Assistant
        | SegmentKind::Thinking
        | SegmentKind::ToolInline
        | SegmentKind::Done => (None, None),
    }
}

/// Builds ratatui lines for a non-Tool, non-collapsed-Thinking message at the
/// given width, returning the lines and search text. Shared by
/// `rebuild_line_cache` (new messages) and `reflow_text_segment` (stale-on-resize
/// messages) so both paths produce identical segments.
fn build_message_lines(msg: &DisplayMessage, width: u16, diagram_pans: Vec<u16>) -> BuiltMessage {
    let width = SegmentChrome::for_kind(segment_kind(&msg.role), width, 0).content_width(width);
    let style = match &msg.role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Tool(_) => unreachable!(),
    };
    let prefix = if msg.plan_path.is_some() {
        ""
    } else {
        style.prefix
    };
    if let Some(notes) = matches!(msg.role, DisplayRole::User)
        .then(|| review::parse(&msg.text))
        .flatten()
    {
        let (lines, provenance, links) = review::card_lines(&notes, width, style.text_style);
        return BuiltMessage {
            search_text: review_search_text(&notes),
            links,
            lines,
            provenance: Some(Provenance::new(msg.text.as_str().into(), provenance)),
            diagrams: Vec::new(),
        };
    }
    let (mut lines, mut provenance, mut diagrams, mut links) = if style.use_markdown {
        let (painted, parsed) = text_to_painted(
            &msg.text,
            prefix,
            style.text_style,
            style.prefix_style,
            width,
            style.max_line_bytes,
            diagram_pans,
        );
        (
            painted.lines,
            Some(Provenance::new(parsed, painted.provenance)),
            painted.diagrams,
            painted.links,
        )
    } else {
        let lines = plain_lines(&msg.text, prefix, style.text_style, style.prefix_style);
        let links = LinkMap::none_for(&lines);
        (lines, None, Vec::new(), links)
    };
    if let Some(pp) = &msg.plan_path {
        // Plan messages splice in rules and a footer, so the recorded line
        // indices no longer match and provenance is dropped.
        provenance = None;
        diagrams.clear();
        if !msg.text.is_empty() {
            let rule = hr_line(width, theme::current().plan_rule);
            lines.insert(0, rule.clone());
            links.rows.insert(0, vec![None; rule.spans.len()]);
            lines.push(rule);
            links
                .rows
                .push(vec![None; lines.last().unwrap().spans.len()]);
        } else {
            lines.clear();
            links.rows.clear();
        }
        if !msg.text.is_empty() {
            lines.push(Line::from(""));
            links
                .rows
                .push(vec![None; lines.last().unwrap().spans.len()]);
        }
        lines.push(Line::from(Span::styled(
            pp.to_owned(),
            theme::current().plan_path,
        )));
        links
            .rows
            .push(vec![None; lines.last().unwrap().spans.len()]);
        lines.push(Line::from(Span::styled(
            format!(
                "{} to open in editor ($VISUAL / $EDITOR)",
                key::OPEN_EDITOR.label
            ),
            theme::current().tool_dim,
        )));
        links
            .rows
            .push(vec![None; lines.last().unwrap().spans.len()]);
    }
    let search_text = format!("{prefix}{}", msg.text);
    BuiltMessage {
        lines,
        search_text,
        provenance,
        diagrams,
        links,
    }
}

struct BuiltMessage {
    lines: Vec<Line<'static>>,
    search_text: String,
    provenance: Option<Provenance>,
    diagrams: Vec<DiagramSpan>,
    links: LinkMap,
}

/// Search should reach what the card shows, not the tags behind it.
fn review_search_text(notes: &[review::ParsedNote]) -> String {
    notes
        .iter()
        .map(review::ParsedNote::search_text)
        .collect::<Vec<_>>()
        .join("\n")
}
