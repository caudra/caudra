pub(crate) mod btw_modal;
pub(crate) mod code_view;
pub mod command;
pub(crate) mod command_modal;
pub(crate) mod file_picker;
pub(crate) mod form;
pub(crate) mod goal_modal;
pub(crate) mod help_modal;
pub mod input;
pub mod keybindings;
pub(crate) mod list_picker;
pub(crate) mod login_picker;
pub(crate) mod lua_float;
pub(crate) mod mcp_picker;
pub(crate) mod memory_picker;
pub(crate) mod message_actions;
pub mod messages;
pub(crate) mod modal;
pub(crate) mod model_picker;
pub(crate) mod paste_editor;
pub(crate) mod permission_prompt;
pub(crate) mod permissions_picker;
pub(crate) mod plan_form;
pub(crate) mod progress_bar;
pub(crate) mod prompt_profile_picker;
pub(crate) mod question_form;
pub mod queue_panel;
pub(crate) mod review;
pub(crate) mod rewind_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod session_picker;
pub(crate) mod split_layout;
pub(crate) mod stash_picker;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod task_picker;
pub(crate) mod theme_picker;
pub(crate) mod todo_panel;
pub(crate) mod tool_display;
pub(crate) mod usage_modal;
pub(crate) mod workbench;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::AgentInput;
use caudra_agent::tools::{SHELL_TOOL_NAME, ToolEffect};
use caudra_agent::{BufferSnapshot, ImageSource, SubagentProgress, ToolInput, ToolOutput};
use caudra_providers::model_registry::{CompactionTarget, GoalEvaluatorTarget, TitleTarget};
use caudra_providers::{CaudraId, HistoryItem, ModelTier};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::selection::wrap_breaks;

pub(crate) const CHEVRON: &str = "❯ ";

pub(crate) fn chevron_span() -> ratatui::text::Span<'static> {
    ratatui::text::Span::styled(CHEVRON, crate::theme::current().tool_dim)
}

/// Style for text the user has typed into a prompt or search field.
///
/// Stated explicitly rather than left at `Style::default()`, whose unset
/// foreground resolves to the terminal's default color instead of the theme's.
pub(crate) fn input_text_style() -> Style {
    Style::new().fg(crate::theme::current().foreground)
}

/// A single-line prompt with the cursor painted onto the cell it occupies.
/// The terminal cursor never moves, so end-of-line needs a space to style.
pub(crate) fn input_line_with_cursor(input: &crate::text_buffer::TextBuffer) -> Line<'static> {
    let value = input.value();
    let cursor_byte = crate::text_buffer::TextBuffer::char_to_byte(&value, input.x());
    let (before, rest) = value.split_at(cursor_byte);
    let mut chars = rest.chars();
    let cursor_char = chars.next().unwrap_or(' ');
    let text = input_text_style();
    Line::from(vec![
        chevron_span(),
        Span::styled(before.to_string(), text),
        Span::styled(cursor_char.to_string(), crate::theme::current().cursor),
        Span::styled(chars.as_str().to_string(), text),
    ])
}

pub(crate) trait Overlay {
    fn is_open(&self) -> bool;
    fn close(&mut self);
    /// Modal overlays block mouse interaction behind them.
    fn is_modal(&self) -> bool {
        true
    }
    /// Override when the overlay draws something that moves on the clock
    /// alone, like a spinner or a reveal. `App::cadence` asks every overlay,
    /// so this is the only place one has to say so.
    fn cadence(&self) -> crate::repaint::Cadence {
        crate::repaint::Cadence::IDLE
    }
}

/// Leading gap before each hint, and the gap between a key and its label.
const HINT_GAP: u16 = 2;
const HINT_KEY_GAP: u16 = 1;

/// One hint's spans and the cells they occupy. Drawing, hit testing and hover
/// all read this, so a hint cannot be styled in one place and measured in
/// another, and a hit rect cannot drift off the glyphs it claims to cover.
fn hint_parts<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Vec<(Vec<Span<'static>>, u16)> {
    let t = crate::theme::current();
    pairs
        .iter()
        .map(|(key, desc)| {
            let mut spans = vec![Span::raw("  ")];
            for (i, part) in key.as_ref().split('/').enumerate() {
                if i > 0 {
                    spans.push(Span::styled("/", t.tool_dim));
                }
                spans.push(Span::styled(part.to_string(), t.keybind_key));
            }
            spans.push(Span::styled(format!(" {}", desc.as_ref()), t.tool_dim));
            let key_width = UnicodeWidthStr::width(key.as_ref()) as u16;
            let desc_width = UnicodeWidthStr::width(desc.as_ref()) as u16;
            (spans, HINT_GAP + key_width + HINT_KEY_GAP + desc_width)
        })
        .collect()
}

pub(crate) fn hint_line<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    hint_line_hovered(pairs, None)
}

/// The hint bar with one pair marked. A hovered hint reverses whole, key and
/// description together, so the pointer marks the control rather than half of
/// it.
pub(crate) fn hint_line_hovered<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
    hovered: Option<usize>,
) -> Line<'static> {
    let spans = hint_parts(pairs)
        .into_iter()
        .enumerate()
        .flat_map(|(index, (spans, _))| {
            let on = hovered == Some(index);
            spans.into_iter().map(move |mut span| {
                span.style = hover_style(span.style, on);
                span
            })
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Where each hint pair landed inside `area`, so a click can name the one it
/// hit. Pairs that run past the right edge are dropped rather than clipped:
/// a hint the reader cannot fully see is not one they can knowingly press.
pub(crate) fn hint_hits<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
    area: ratatui::layout::Rect,
) -> Vec<ratatui::layout::Rect> {
    let mut x = area.x;
    let mut hits = Vec::with_capacity(pairs.len());
    for (_, width) in hint_parts(pairs) {
        if x.saturating_add(width) > area.right() {
            break;
        }
        hits.push(ratatui::layout::Rect {
            x,
            y: area.y,
            width,
            height: 1,
        });
        x += width;
    }
    hits
}

/// Where each logical line starts once wrapping has been applied, so a hit
/// rect can be placed on a line the reader sees rather than the one it was
/// written as.
struct VisualRows {
    starts: Vec<u16>,
    total: u16,
}

impl VisualRows {
    fn row_of(&self, line: u16) -> u16 {
        self.starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.total)
    }

    fn height_of(&self, line: u16) -> u16 {
        self.row_of(line + 1).saturating_sub(self.row_of(line))
    }
}

/// A prefixed line pre-wrapped so every row after the first hangs under the
/// text instead of restarting at column zero, which is all `Wrap` can do. The
/// wrap points come from [`wrap_breaks`], which already replays ratatui's
/// algorithm, so the rows this hands back are the rows the widget would draw.
pub(crate) fn hanging_lines(
    prefix: Span<'static>,
    text: Span<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let indent = UnicodeWidthStr::width(prefix.content.as_ref()) as u16;
    let chars: Vec<char> = text.content.chars().collect();
    let mut starts = vec![0];
    starts.extend(
        wrap_breaks(&chars, width.saturating_sub(indent).max(1))
            .into_iter()
            .map(|brk| brk.start),
    );
    let hang = Span::styled(" ".repeat(usize::from(indent)), prefix.style);
    starts
        .iter()
        .enumerate()
        .map(|(row, &start)| {
            let end = starts.get(row + 1).copied().unwrap_or(chars.len());
            // Trailing spaces are what the wrap broke on; keeping them could
            // push a row past the width it was measured for.
            let content: String = chars[start..end].iter().collect();
            let content = match end == chars.len() {
                true => content,
                false => content.trim_end().to_owned(),
            };
            Line::from(vec![
                match row {
                    0 => prefix.clone(),
                    _ => hang.clone(),
                },
                Span::styled(content, text.style),
            ])
        })
        .collect()
}

/// Measured with the same widget that draws them, so the two can never
/// disagree about where a wrap falls.
fn visual_rows(lines: &[Line<'static>], width: u16) -> VisualRows {
    let width = width.max(1);
    let mut starts = Vec::with_capacity(lines.len());
    let mut total = 0;
    for line in lines {
        starts.push(total);
        total += Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
    }
    VisualRows { starts, total }
}

/// How every control in the UI says the pointer is on it. One helper so a new
/// button cannot invent its own idea of what hovered looks like.
pub(crate) fn hover_style(style: Style, hovered: bool) -> Style {
    if hovered {
        style.add_modifier(ratatui::style::Modifier::REVERSED)
    } else {
        style
    }
}

pub(crate) fn visual_line_count(text_len: usize, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    text_len.div_ceil(width).max(1)
}

pub(crate) fn apply_scroll_delta(offset: u16, delta: i32) -> u16 {
    if delta > 0 {
        offset.saturating_sub(delta as u16)
    } else {
        offset.saturating_add(delta.unsigned_abs() as u16)
    }
}

pub(crate) fn escape_terminal_controls(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

pub fn is_ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

pub(crate) struct ModalScroll {
    offset: u16,
    max_offset: u16,
    viewport_h: u16,
    auto_scroll: bool,
}

impl ModalScroll {
    pub fn new() -> Self {
        Self {
            offset: 0,
            max_offset: 0,
            viewport_h: 0,
            auto_scroll: true,
        }
    }

    pub fn new_top() -> Self {
        Self {
            auto_scroll: false,
            ..Self::new()
        }
    }

    pub fn reset(&mut self) {
        let auto_scroll = self.auto_scroll;
        *self = Self::new();
        self.auto_scroll = auto_scroll;
    }

    pub fn offset(&self) -> u16 {
        self.offset
    }

    pub fn update_dimensions(&mut self, total: u16, viewport_h: u16) {
        self.viewport_h = viewport_h;
        self.max_offset = total.saturating_sub(viewport_h);
        if self.auto_scroll {
            self.offset = self.max_offset;
        } else {
            self.clamp();
            if self.offset >= self.max_offset {
                self.auto_scroll = true;
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.offset = apply_scroll_delta(self.offset, delta);
        self.clamp();
        self.auto_scroll = self.offset >= self.max_offset;
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        use keybindings::key;
        match key_event.code {
            KeyCode::Up => self.scroll(1),
            KeyCode::Down => self.scroll(-1),
            _ if key::SCROLL_HALF_UP.matches(key_event)
                || key::SCROLL_HALF_UP_ALT.matches(key_event) =>
            {
                self.scroll(self.half_page())
            }
            _ if key::SCROLL_HALF_DOWN.matches(key_event) => self.scroll(-self.half_page()),
            _ if key::SCROLL_LINE_UP.matches(key_event) => self.scroll(1),
            _ if key::SCROLL_LINE_DOWN.matches(key_event) => self.scroll(-1),
            _ if key::SCROLL_TOP.matches(key_event) || key::SCROLL_TOP_ALT.matches(key_event) => {
                self.offset = 0;
                self.auto_scroll = false;
            }
            _ if key::SCROLL_BOTTOM.matches(key_event)
                || key::SCROLL_BOTTOM_ALT.matches(key_event) =>
            {
                self.auto_scroll = true;
                self.offset = self.max_offset;
            }
            _ => return false,
        }
        true
    }

    fn half_page(&self) -> i32 {
        (self.viewport_h / 2).max(1) as i32
    }

    fn clamp(&mut self) {
        self.offset = self.offset.min(self.max_offset);
    }
}

pub struct LoadedSession {
    pub messages: Vec<HistoryItem>,
    pub model_spec: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionProvider {
    Anthropic,
    OpenAi,
}

impl SubscriptionProvider {
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
        }
    }
}

use std::path::PathBuf;

pub enum Action {
    SendMessage(Box<AgentInput>),
    ManualExit,
    ShellCommand {
        id: String,
        command: String,
        visible: bool,
    },
    CancelAgent {
        run_id: u64,
    },
    CancelSubagent {
        tool_use_id: String,
    },
    RequestNewSession,
    NewSession(Arc<caudra_storage::sessions::SessionLease>),
    LoadSession(Box<LoadedSession>),
    ForkSession(Box<ForkedSession>),
    RevertSession {
        source: DisplaySource,
        mode: RestoreMode,
    },
    RewindSession(rewind_picker::RewindEntry),
    UnrevertSession,
    ChangeWorkingDirectory(PathBuf),
    ChangeModel(String),
    ChangeSystemPromptProfile(String),
    RefreshProvider {
        slug: String,
    },
    AuthenticateProvider {
        provider: SubscriptionProvider,
        model_spec: String,
    },
    AssignTier(String, ModelTier),
    ResetTier(ModelTier),
    SetGoalEvaluator(GoalEvaluatorTarget),
    SetCompaction(CompactionTarget),
    SetTitleModel(TitleTarget),
    RefreshModels,
    RefreshUsage,
    Compact,
    ToggleMcp(String, bool),
    TrustMcpOnce(String),
    TrustMcpProject(String),
    RejectMcp(String),
    FocusSession(caudra_storage::id::CaudraId),
    DeleteSession(caudra_storage::id::CaudraId),
    SetSessionTitle {
        id: caudra_storage::id::CaudraId,
        title: String,
    },
    OpenEditor(PathBuf),
    OpenUrl(String),
    EditInputInEditor,
    Btw(String),
    Suspend,
}

pub struct ForkedSession {
    pub session: crate::AppSession,
    pub lease: Arc<caudra_storage::sessions::SessionLease>,
    pub draft: Option<ForkDraft>,
}

pub struct ForkDraft {
    pub text: String,
    pub images: Vec<ImageSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    Conversation,
    Files,
    Both,
}

impl RestoreMode {
    pub(crate) fn restores_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }

    pub(crate) fn restores_files(self) -> bool {
        matches!(self, Self::Files | Self::Both)
    }
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitRequest {
    #[default]
    None,
    Success,
    Error,
    Reload,
}

impl ExitRequest {
    pub fn code(&self) -> ExitCode {
        match self {
            Self::None | Self::Success | Self::Reload => ExitCode::SUCCESS,
            Self::Error => ExitCode::FAILURE,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Status {
    Idle,
    Streaming,
    Error { message: String, since: Instant },
}

impl Status {
    pub fn error(message: String) -> Self {
        Self::Error {
            message,
            since: Instant::now(),
        }
    }

    pub fn is_error_expired(&self) -> bool {
        matches!(self, Self::Error { since, .. } if since.elapsed() >= ERROR_DISPLAY)
    }
}

impl PartialEq for Status {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Idle, Self::Idle)
                | (Self::Streaming, Self::Streaming)
                | (Self::Error { .. }, Self::Error { .. })
        )
    }
}

pub struct RetryInfo {
    pub attempt: u32,
    pub message: String,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolStatus {
    InProgress,
    Success,
    Error,
}

/// A subagent's last progress report, plus when it landed so the row keeps
/// counting between reports. A report only moves on a state change, and a
/// single tool call can run for minutes.
#[derive(Debug, Clone)]
pub struct ToolProgress {
    pub report: SubagentProgress,
    /// `None` once the call ended and the report is the final word.
    since: Option<Instant>,
}

impl ToolProgress {
    pub fn live(report: SubagentProgress) -> Self {
        Self {
            report,
            since: Some(Instant::now()),
        }
    }

    pub fn is_live(&self) -> bool {
        self.since.is_some()
    }

    pub fn settle(&mut self) {
        self.report.elapsed = self.elapsed();
        self.since = None;
    }

    pub fn elapsed(&self) -> Duration {
        self.report.elapsed + self.since.map_or(Duration::ZERO, |since| since.elapsed())
    }
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub text: String,
    pub source: Option<DisplaySource>,
    pub tool_input: Option<Arc<ToolInput>>,
    pub tool_raw_input: Option<Arc<serde_json::Value>>,
    pub tool_output: Option<Arc<ToolOutput>>,
    pub live_output: Option<String>,
    pub annotation: Option<String>,
    /// How the subagent behind this tool call is getting on. Absent for every
    /// other tool, and for a restored one: it is live chrome, like
    /// [`Self::turn_usage`].
    pub progress: Option<ToolProgress>,
    pub plan_path: Option<String>,
    pub timestamp: Option<String>,
    pub turn_usage: Option<String>,
    pub truncated_lines: usize,
    pub render_snapshot: Option<BufferSnapshot>,
    pub render_header: Option<BufferSnapshot>,
    pub snapshot_theme_gen: u64,
    /// What the reader asked of this reasoning block. `None` leaves the
    /// disclosure to the view mode, which is where every card starts.
    pub reasoning_open: Option<bool>,
    /// Wall time the model spent on a `Thinking` block. Absent for sessions
    /// written before reasoning was timed.
    pub thinking_duration: Option<Duration>,
}

impl DisplayMessage {
    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
            thinking_duration: None,
        }
    }

    pub fn plan(text: String, plan_path: String) -> Self {
        Self {
            role: DisplayRole::Assistant,
            text,
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: Some(plan_path),
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
            thinking_duration: None,
        }
    }

    pub fn snapshot_is_stale(&self, current_gen: u64) -> bool {
        (self.render_snapshot.is_some() || self.render_header.is_some())
            && self.snapshot_theme_gen != current_gen
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaySource {
    User(CaudraId),
    AssistantText(CaudraId),
    Reasoning(CaudraId),
    ToolCall {
        id: CaudraId,
        result_id: Option<CaudraId>,
    },
    ToolResult(CaudraId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRole {
    pub id: String,
    pub status: ToolStatus,
    pub name: Arc<str>,
    /// What the call was allowed to do, taken from the call itself rather
    /// than looked up at render time, so a transcript reads the same however
    /// long after the run it is opened.
    pub effect: ToolEffect,
}

impl ToolRole {
    pub fn is_collapsible(&self) -> bool {
        is_collapsible(self.effect, &self.name)
    }
}

/// Whether a call can be put away behind its header without losing the record
/// of what it did.
///
/// `ToolEffect` alone cannot answer this. It says whether a call may change
/// something, which is what permissions need, and a write and a shell command
/// are both `Mutating`. What matters here is where the record lives. A write's
/// diff exists nowhere but the body, so hiding it loses the change. A shell
/// command is named in full by its own header, down to its exit status, and
/// what its body holds is what the command printed rather than what it did.
///
/// Taken loose from `ToolRole` because a batch child is the same call without
/// a card of its own, and it has to fold by the same rule or the two drift.
pub(crate) fn is_collapsible(effect: ToolEffect, tool: &str) -> bool {
    effect.is_collapsible() || tool == SHELL_TOOL_NAME
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    Thinking,
    Tool(Box<ToolRole>),
    Error,
    Done,
}

impl DisplayRole {
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            DisplayRole::Tool(t) => Some(&t.name),
            _ => None,
        }
    }

    pub fn tool_id(&self) -> Option<&str> {
        match self {
            DisplayRole::Tool(t) => Some(&t.id),
            _ => None,
        }
    }
}

#[cfg(test)]
use caudra_providers::ModelPricing;

#[cfg(test)]
pub(crate) const TEST_CONTEXT_WINDOW: u32 = 200_000;

#[cfg(test)]
pub(crate) fn test_pricing() -> ModelPricing {
    ModelPricing {
        input: 3.0,
        output: 15.0,
        cache_write: 3.75,
        cache_read: 0.30,
        fast: None,
        tiers: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) fn test_model() -> caudra_providers::Model {
    caudra_providers::Model {
        id: "test-model".into(),
        provider: std::sync::Arc::<str>::from("anthropic"),
        tier: caudra_providers::ModelTier::Medium,
        family: caudra_providers::ModelFamily::Claude,
        supports_tool_examples_override: None,
        thinking_override: None,
        supports_vision_override: Some(true),
        pricing: test_pricing(),
        discovered_free: false,
        max_output_tokens: Some(8192),
        context_window: TEST_CONTEXT_WINDOW,
        reasoning_options: caudra_providers::ReasoningOptions::default(),
        thinking_fields: None,
    }
}

/// Every symbol in the buffer, row by row, for `assert!(screen.contains(..))`.
#[cfg(test)]
pub(crate) fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    buf.content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[cfg(test)]
pub(crate) fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent {
        code,
        modifiers: crossterm::event::KeyModifiers::NONE,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::{SnapshotLine, SnapshotSpan, SpanStyle};
    use test_case::test_case;

    const SNAPSHOT_GEN: u64 = 7;

    fn snapshot() -> BufferSnapshot {
        BufferSnapshot::from_arc(Arc::new(vec![SnapshotLine {
            spans: vec![SnapshotSpan {
                text: "baked".into(),
                style: SpanStyle::Default,
            }],
        }]))
    }

    #[test_case(false, false, false => false ; "no_snapshot_never_stale")]
    #[test_case(true,  false, true  => false ; "has_snapshot_matching_gen_fresh")]
    #[test_case(true,  false, false => true  ; "has_snapshot_mismatched_gen_stale")]
    fn snapshot_is_stale_cases(has_body: bool, has_header: bool, gen_match: bool) -> bool {
        let mut msg = DisplayMessage::new(DisplayRole::Assistant, "hi".into());
        msg.snapshot_theme_gen = SNAPSHOT_GEN;
        if has_body {
            msg.render_snapshot = Some(snapshot());
        }
        if has_header {
            msg.render_header = Some(snapshot());
        }
        let current_gen = if gen_match {
            SNAPSHOT_GEN
        } else {
            SNAPSHOT_GEN + 1
        };
        msg.snapshot_is_stale(current_gen)
    }

    #[test_case(0, 80, 1 ; "empty_text")]
    #[test_case(0, 0, 1 ; "zero_width")]
    #[test_case(5, 5, 1 ; "exact_fit")]
    #[test_case(6, 5, 2 ; "one_char_overflow")]
    fn visual_line_count_cases(text_len: usize, width: usize, expected: usize) {
        assert_eq!(visual_line_count(text_len, width), expected);
    }

    #[test_case(10, 3, 7   ; "scroll_up")]
    #[test_case(10, -3, 13 ; "scroll_down")]
    #[test_case(0, 5, 0    ; "clamp_underflow")]
    fn apply_scroll_delta_cases(offset: u16, delta: i32, expected: u16) {
        assert_eq!(apply_scroll_delta(offset, delta), expected);
    }

    const MODAL_TOTAL: u16 = 100;
    const MODAL_VIEWPORT: u16 = 20;
    const MODAL_MAX_OFFSET: u16 = MODAL_TOTAL - MODAL_VIEWPORT;
    const MODAL_HALF_PAGE: u16 = MODAL_VIEWPORT / 2;

    const HANG_PREFIX: &str = "--> ";
    const HANG_WIDTH: u16 = 12;

    /// The rows `hanging_lines` builds, as text, so a test can read them the
    /// way the terminal draws them.
    fn hung(text: &str) -> Vec<String> {
        hanging_lines(
            Span::raw(HANG_PREFIX),
            Span::raw(text.to_owned()),
            HANG_WIDTH,
        )
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
    }

    #[test_case("one two",          &["--> one two"]                     ; "fits_on_one_row")]
    #[test_case("one two three",    &["--> one two", "    three"]        ; "wraps_at_a_word")]
    #[test_case("one    two",       &["--> one", "    two"]              ; "drops_the_spaces_it_broke_on")]
    #[test_case("aaaaaaaaaaaa",     &["--> aaaaaaaa", "    aaaa"]        ; "breaks_a_word_wider_than_the_body")]
    fn hanging_lines_hang_under_the_first_row(text: &str, expected: &[&str]) {
        assert_eq!(hung(text), expected);
    }

    /// Measured with the widget that draws them, so a row that is one cell too
    /// wide would wrap again and land back in column zero.
    #[test]
    fn every_hung_row_fits_the_width_it_was_wrapped_for() {
        let lines = hanging_lines(
            Span::raw(HANG_PREFIX),
            Span::raw("one two three four five sixsixsixsixsix".to_owned()),
            HANG_WIDTH,
        );
        let rows = visual_rows(&lines, HANG_WIDTH);
        assert_eq!(rows.total as usize, lines.len(), "no row wrapped twice");
    }

    /// A prefix with no room left for text still has to hand back a row per
    /// character rather than dividing by zero.
    #[test]
    fn a_prefix_as_wide_as_the_body_still_wraps() {
        let lines = hanging_lines(
            Span::raw(HANG_PREFIX),
            Span::raw("ab".to_owned()),
            HANG_PREFIX.len() as u16,
        );
        assert_eq!(lines.len(), 2);
    }

    /// Modals answer the same navigation keys as the transcript.
    #[test_case(keybindings::key::SCROLL_TOP_ALT.to_key_event(),     0                                  ; "ctrl_home")]
    #[test_case(keybindings::key::SCROLL_BOTTOM_ALT.to_key_event(),  MODAL_MAX_OFFSET                   ; "ctrl_end")]
    #[test_case(keybindings::key::SCROLL_HALF_UP_ALT.to_key_event(), MODAL_MAX_OFFSET - MODAL_HALF_PAGE ; "page_up")]
    #[test_case(keybindings::key::SCROLL_HALF_DOWN.to_key_event(),   MODAL_MAX_OFFSET                   ; "page_down")]
    fn modal_scroll_navigation_keys(key_event: KeyEvent, expected: u16) {
        let mut scroll = ModalScroll::new();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);

        assert!(scroll.handle_key(key_event));
        assert_eq!(scroll.offset(), expected);
    }
}
