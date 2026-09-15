pub(crate) mod btw_modal;
pub(crate) mod code_view;
pub mod command;
pub(crate) mod command_modal;
pub(crate) mod context_modal;
pub(crate) mod environment_card;
pub(crate) mod file_picker;
pub(crate) mod file_walk;
pub(crate) mod form;
pub(crate) mod goal_modal;
pub(crate) mod help_modal;
pub mod input;
pub mod keybindings;
pub(crate) mod list_picker;
pub(crate) mod login_picker;
pub(crate) mod logs_modal;
pub(crate) mod lua_float;
pub(crate) mod mcp_picker;
pub(crate) mod memory_picker;
pub(crate) mod mention_popup;
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
pub(crate) mod queue_actions;
pub mod queue_panel;
pub(crate) mod review;
pub(crate) mod rewind_picker;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod session_picker;
pub(crate) mod session_relocation;
pub(crate) mod skills_modal;
pub(crate) mod split_layout;
pub(crate) mod stash_picker;
pub mod status_bar;
pub(crate) mod storage_modal;
pub(crate) mod streaming_content;
pub(crate) mod task_picker;
pub(crate) mod text_editor;
pub(crate) mod theme_picker;
pub(crate) mod thinking_picker;
pub(crate) mod todo_panel;
pub(crate) mod tool_display;
pub(crate) mod tools_modal;
pub(crate) mod usage_modal;
pub(crate) mod which_key;
pub(crate) mod workbench;
pub(crate) mod workflow_card;
pub(crate) mod workflow_catalog_picker;
pub(crate) mod workflow_inspector;

use std::iter;
use std::mem;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::AgentInput;
use caudra_agent::tools::{SHELL_TOOL_NAME, ToolEffect};
use caudra_agent::{BufferSnapshot, ImageSource, SubagentProgress, ToolInput, ToolOutput};
use caudra_providers::model_registry::Binding;
use caudra_providers::{CaudraId, HistoryItem, ModelPurpose};
use caudra_storage::sessions::SessionRelocation;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::selection::wrap_breaks;

pub(crate) const CHEVRON: &str = "❯ ";
const DIGIT_GROUP: usize = 3;
/// Columns a modal pans per key press. Roughly one column of a token table, so
/// a reader walks the table a field at a time rather than a glyph at a time.
const PAN_STEP: i32 = 8;

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

/// The runs of `text` a fuzzy search matched, painted apart from the rest of it.
///
/// `indices` are character positions into `text`, ascending, as every nucleo
/// matcher reports them. `max_width` is a display-column budget, so a row of
/// wide glyphs stops at the same place a row of narrow ones does; the caller
/// supplies any indent or padding around what comes back.
pub(crate) fn match_spans(
    text: &str,
    indices: &[u32],
    base: Style,
    matched: Style,
    max_width: Option<usize>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut in_match = false;
    let mut run = String::new();
    let mut width = 0usize;

    for (i, ch) in text.chars().enumerate() {
        if let Some(budget) = max_width {
            let cw = ch.width().unwrap_or(0);
            if width + cw > budget {
                break;
            }
            width += cw;
        }

        let is_match = indices.binary_search(&(i as u32)).is_ok();
        if is_match != in_match && !run.is_empty() {
            let style = if in_match { matched } else { base };
            spans.push(Span::styled(mem::take(&mut run), style));
        }
        in_match = is_match;
        run.push(ch);
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, if in_match { matched } else { base }));
    }

    spans
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
const HINT_GAP: &str = "  ";
const HINT_KEY_GAP: &str = " ";

/// One hint's spans and the cells they occupy, the gap before it excluded.
/// Drawing, hit testing and hover all read this, so a hint cannot be styled in
/// one place and measured in another, and a hit rect cannot drift off the
/// glyphs it claims to cover.
fn hint_parts<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Vec<(Vec<Span<'static>>, u16)> {
    let t = crate::theme::current();
    pairs
        .iter()
        .map(|(key, desc)| {
            let mut spans = Vec::new();
            for (i, part) in key.as_ref().split('/').enumerate() {
                if i > 0 {
                    spans.push(Span::styled("/", t.tool_dim));
                }
                spans.push(Span::styled(part.to_string(), t.keybind_key));
            }
            spans.push(Span::styled(
                format!("{HINT_KEY_GAP}{}", desc.as_ref()),
                t.tool_dim,
            ));
            let width = UnicodeWidthStr::width(key.as_ref())
                + UnicodeWidthStr::width(HINT_KEY_GAP)
                + UnicodeWidthStr::width(desc.as_ref());
            (spans, width as u16)
        })
        .collect()
}

fn hint_gap_width() -> u16 {
    UnicodeWidthStr::width(HINT_GAP) as u16
}

pub(crate) fn hint_line<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    hint_line_hovered(pairs, None)
}

/// The hint bar with one pair marked. A hovered hint reverses whole, key and
/// description together, so the pointer marks the control rather than half of
/// it. The gap before a hint separates two controls and belongs to neither, so
/// it is drawn plain however the pointer moves.
pub(crate) fn hint_line_hovered<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
    hovered: Option<usize>,
) -> Line<'static> {
    let spans = hint_parts(pairs)
        .into_iter()
        .enumerate()
        .flat_map(|(index, (spans, _))| {
            let on = hovered == Some(index);
            iter::once(Span::raw(HINT_GAP)).chain(spans.into_iter().map(move |mut span| {
                span.style = hover_style(span.style, on);
                span
            }))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Where each hint pair landed inside `area`, so a click can name the one it
/// hit. The rect covers the hint's own glyphs and not the gap that precedes
/// it, so the pointer acts on a control only once it is over one. Pairs that
/// run past the right edge are dropped rather than clipped: a hint the reader
/// cannot fully see is not one they can knowingly press.
pub(crate) fn hint_hits<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)], area: Rect) -> Vec<Rect> {
    let mut x = area.x;
    let mut hits = Vec::with_capacity(pairs.len());
    for (_, width) in hint_parts(pairs) {
        x = x.saturating_add(hint_gap_width());
        if x.saturating_add(width) > area.right() {
            break;
        }
        hits.push(Rect {
            x,
            y: area.y,
            width,
            height: 1,
        });
        x += width;
    }
    hits
}

/// The area a modal's horizontal bar is handed: its body plus the border row
/// under it. The bar paints on the last row alone, so it costs the body
/// nothing, and the rows it is handed over that are what a fingertip's hit
/// margin is taken from. Handing over the border row by itself paints the same
/// bar and leaves touch with a one-row target it cannot hit.
pub(crate) fn bar_area(body: Rect) -> Rect {
    Rect {
        height: body.height.saturating_add(1),
        ..body
    }
}

/// Where each logical line starts once wrapping has been applied, so a hit
/// rect can be placed on a line the reader sees rather than the one it was
/// written as.
pub(crate) struct VisualRows {
    starts: Vec<u16>,
    pub(crate) total: u16,
}

impl VisualRows {
    pub(crate) fn row_of(&self, line: u16) -> u16 {
        self.starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.total)
    }

    pub(crate) fn height_of(&self, line: u16) -> u16 {
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
pub(crate) fn visual_rows(lines: &[Line<'static>], width: u16) -> VisualRows {
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

/// A positive delta scrolls towards the top of the document, so it lowers the
/// offset. The transcript counts rows in `u32` because a long session runs
/// past 65535 of them; every other surface is bounded by its own content and
/// stays in `u16`.
pub(crate) fn apply_scroll_rows(offset: u32, delta: i32) -> u32 {
    if delta > 0 {
        offset.saturating_sub(delta.unsigned_abs())
    } else {
        offset.saturating_add(delta.unsigned_abs())
    }
}

pub(crate) fn apply_scroll_delta(offset: u16, delta: i32) -> u16 {
    apply_scroll_rows(u32::from(offset), delta).min(u32::from(u16::MAX)) as u16
}

/// Splits `cells` between `weights` by largest remainder, so the parts always
/// sum to `cells` and a non-zero weight is never rounded away to nothing on the
/// proportional bars the modals draw.
pub(crate) fn apportion(weights: &[u64], cells: usize) -> Vec<usize> {
    let total = weights.iter().copied().fold(0_u64, u64::saturating_add);
    if total == 0 {
        return vec![0; weights.len()];
    }

    let cell_count = u64::try_from(cells).unwrap_or(u64::MAX);
    let mut allocated = 0_usize;
    let mut remainders = Vec::with_capacity(weights.len());
    let mut result = weights
        .iter()
        .map(|weight| {
            let numerator = weight.saturating_mul(cell_count);
            let count = usize::try_from(numerator / total).unwrap_or(usize::MAX);
            allocated = allocated.saturating_add(count);
            remainders.push(numerator % total);
            count
        })
        .collect::<Vec<_>>();
    let mut order = (0..weights.len()).collect::<Vec<_>>();
    order.sort_unstable_by(|left, right| {
        remainders[*right]
            .cmp(&remainders[*left])
            .then_with(|| left.cmp(right))
    });
    for index in order.into_iter().take(cells.saturating_sub(allocated)) {
        result[index] = result[index].saturating_add(1);
    }
    result
}

pub(crate) fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let separators = digits.len().saturating_sub(1) / DIGIT_GROUP;
    let mut grouped = String::with_capacity(digits.len().saturating_add(separators));
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(DIGIT_GROUP) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub(crate) fn format_usize(value: usize) -> String {
    format_integer(u64::try_from(value).unwrap_or(u64::MAX))
}

/// `41k`, `1.2M`: a count at the width a chip or a card row can spare.
pub(crate) fn format_compact(value: u64) -> String {
    const THOUSAND: u64 = 1_000;
    const MILLION: u64 = 1_000_000;
    if value >= MILLION {
        format!("{:.1}M", value as f64 / MILLION as f64)
    } else if value >= THOUSAND {
        format!("{}k", value / THOUSAND)
    } else {
        value.to_string()
    }
}

/// `2m14s`, `1h03m`, `41s`: a span of seconds as a run card reads it.
pub(crate) fn format_elapsed(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3_600;
    if seconds >= HOUR {
        format!("{}h{:02}m", seconds / HOUR, seconds % HOUR / MINUTE)
    } else if seconds >= MINUTE {
        format!("{}m{:02}s", seconds / MINUTE, seconds % MINUTE)
    } else {
        format!("{seconds}s")
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
    pan: u16,
    max_pan: u16,
    auto_scroll: bool,
    default_auto_scroll: bool,
}

impl ModalScroll {
    pub fn new() -> Self {
        Self {
            offset: 0,
            max_offset: 0,
            viewport_h: 0,
            pan: 0,
            max_pan: 0,
            auto_scroll: true,
            default_auto_scroll: true,
        }
    }

    pub fn new_top() -> Self {
        Self {
            auto_scroll: false,
            default_auto_scroll: false,
            ..Self::new()
        }
    }

    pub fn reset(&mut self) {
        self.offset = 0;
        self.max_offset = 0;
        self.viewport_h = 0;
        self.pan = 0;
        self.max_pan = 0;
        self.auto_scroll = self.default_auto_scroll;
    }

    pub fn offset(&self) -> u16 {
        self.offset
    }

    pub fn pan(&self) -> u16 {
        self.pan
    }

    /// How far the widest line runs past the viewport. Only the modals that draw
    /// unwrapped lines call this: everywhere else `max_pan` stays zero, which is
    /// what keeps the pan keys from being swallowed by a modal that reflows and
    /// has nothing off screen to reach.
    pub fn fit_width(&mut self, content_w: u16, viewport_w: u16) {
        self.max_pan = content_w.saturating_sub(viewport_w);
        self.pan = self.pan.min(self.max_pan);
    }

    /// A positive delta moves the view right, towards the end of the line. The
    /// opposite sign convention to [`Self::scroll`], which counts upwards, and
    /// the same one the log pane already pans by.
    pub fn pan_by(&mut self, delta: i32) {
        let pan = i64::from(self.pan) + i64::from(delta);
        self.pan_to(pan.clamp(0, i64::from(u16::MAX)) as u16);
    }

    /// Lands on a column rather than stepping towards one, which is what a
    /// horizontal bar drag hands over.
    pub fn pan_to(&mut self, pan: u16) {
        self.pan = pan.min(self.max_pan);
    }

    pub fn update_dimensions(&mut self, total: u16, viewport_h: u16) {
        self.viewport_h = viewport_h;
        self.max_offset = total.saturating_sub(viewport_h);
        if self.auto_scroll {
            self.offset = self.max_offset;
        } else {
            self.clamp();
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.offset = apply_scroll_delta(self.offset, delta);
        self.clamp();
        self.repin();
    }

    /// Lands on a row rather than stepping towards one, which is what a
    /// scrollbar drag hands over. Re-pins at the bottom like a wheel does, so
    /// dragging to the end resumes following a growing modal.
    pub fn scroll_to(&mut self, offset: u16) {
        self.offset = offset;
        self.clamp();
        self.repin();
    }

    fn repin(&mut self) {
        if self.max_offset > 0 {
            self.auto_scroll = self.offset >= self.max_offset;
        }
    }

    /// Scrolls the least distance that brings a row range into view, so moving
    /// a selection past the edge follows it instead of jumping.
    pub fn reveal(&mut self, top: u16, height: u16) {
        if top < self.offset {
            self.offset = top;
        } else if top.saturating_add(height) > self.offset.saturating_add(self.viewport_h) {
            self.offset = top
                .saturating_add(height)
                .saturating_sub(self.viewport_h.max(1));
        }
        self.clamp();
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        use keybindings::key;
        match key_event.code {
            KeyCode::Up => self.scroll(1),
            KeyCode::Down => self.scroll(-1),
            // Claimed only while there is something off screen to reach, so a
            // modal whose lines already fit leaves the chord to the transcript.
            _ if self.max_pan > 0 && key::PAN_LEFT.matches(key_event) => self.pan_by(-PAN_STEP),
            _ if self.max_pan > 0 && key::PAN_RIGHT.matches(key_event) => self.pan_by(PAN_STEP),
            _ if key::SCROLL_HALF_UP.matches(key_event) || key::PAGE_UP.matches(key_event) => {
                self.scroll(self.half_page())
            }
            _ if key::PAGE_DOWN.matches(key_event) => self.scroll(-self.half_page()),
            _ if key::SCROLL_LINE_UP.matches(key_event) => self.scroll(1),
            _ if key::SCROLL_LINE_DOWN.matches(key_event) => self.scroll(-1),
            _ if key::SCROLL_TOP.matches(key_event) || key::DOC_TOP.matches(key_event) => {
                self.offset = 0;
                self.auto_scroll = false;
            }
            _ if key::SCROLL_BOTTOM.matches(key_event) || key::DOC_BOTTOM.matches(key_event) => {
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
    OpenSessionRelocation {
        bulk: bool,
        destination: Option<String>,
    },
    RelocateSessions {
        request: SessionRelocation,
        donor: Option<(CaudraId, String)>,
    },
    ChangeRemoteWorkingDirectory(caudra_workspace::DirectoryNavigation),
    RemoteControl(String),
    ChangeModel(String),
    ChangeSystemPromptProfile(String),
    RefreshProvider {
        slug: String,
    },
    AuthenticateProvider {
        provider: SubscriptionProvider,
        model_spec: String,
    },
    Bind(ModelPurpose, Binding),
    Unbind(ModelPurpose),
    RefreshModels,
    RefreshUsage,
    RefreshStorage,
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
    GenerateSessionTitle(caudra_storage::id::CaudraId),
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
    pub tool_preview_pending: bool,
    pub live_output: Option<String>,
    /// The file a write is still spelling out, as far as its arguments have
    /// arrived. Transient by construction: only ever set while the call's
    /// arguments are still arriving, replaced by the call's real output the
    /// moment the tool starts, and never stored.
    pub live_body: Option<String>,
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
    pub body_open: Option<bool>,
    /// Wall time the model spent on a `Thinking` block. Absent for sessions
    /// written before reasoning was timed.
    pub thinking_duration: Option<Duration>,
    /// When the tool actually began running, which is after the permission
    /// verdict: the clock measures the command, not the prompt in front of it.
    /// Absent on a restored card, which has no live phase to time.
    pub tool_started: Option<Instant>,
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
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
            tool_started: None,
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
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: Some(plan_path),
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
            tool_started: None,
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
    /// Harness chatter: what the run did on the user's behalf, never
    /// something a model or a person said.
    Notice,
    /// A message the harness wrote into the conversation. Unlike a notice it
    /// has a body worth reading, so it collapses to its heading and opens on a
    /// click.
    Injected,
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
        family: caudra_providers::ModelFamily::Claude,
        supports_tool_examples_override: None,
        thinking_override: None,
        supports_vision_override: Some(true),
        pricing: test_pricing(),
        discovered_free: false,
        max_output_tokens: Some(8192),
        context_window: TEST_CONTEXT_WINDOW,
        window_excludes_output: false,
        reasoning_options: caudra_providers::ReasoningOptions::default(),
        thinking_fields: None,
        billing: caudra_providers::Billing::Api,
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
    use ratatui::style::Modifier;
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
    #[test_case(keybindings::key::DOC_TOP.to_key_event(),    0                                  ; "home")]
    #[test_case(keybindings::key::DOC_BOTTOM.to_key_event(), MODAL_MAX_OFFSET                   ; "end")]
    #[test_case(keybindings::key::PAGE_UP.to_key_event(),    MODAL_MAX_OFFSET - MODAL_HALF_PAGE ; "page_up")]
    #[test_case(keybindings::key::PAGE_DOWN.to_key_event(),  MODAL_MAX_OFFSET                   ; "page_down")]
    fn modal_scroll_navigation_keys(key_event: KeyEvent, expected: u16) {
        let mut scroll = ModalScroll::new();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);

        assert!(scroll.handle_key(key_event));
        assert_eq!(scroll.offset(), expected);
    }

    #[test]
    fn modal_scroll_top_default_survives_content_changes_and_reset() {
        let mut scroll = ModalScroll::new_top();
        scroll.update_dimensions(MODAL_VIEWPORT, MODAL_VIEWPORT);
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert_eq!(scroll.offset(), 0);

        assert!(scroll.handle_key(keybindings::key::DOC_BOTTOM.to_key_event()));
        assert_eq!(scroll.offset(), MODAL_MAX_OFFSET);
        scroll.reset();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert_eq!(scroll.offset(), 0);
    }

    #[test]
    fn modal_scroll_bottom_default_survives_content_changes_and_reset() {
        let mut scroll = ModalScroll::new();
        scroll.update_dimensions(MODAL_VIEWPORT, MODAL_VIEWPORT);
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert_eq!(scroll.offset(), MODAL_MAX_OFFSET);

        assert!(scroll.handle_key(keybindings::key::DOC_TOP.to_key_event()));
        assert_eq!(scroll.offset(), 0);
        scroll.reset();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert_eq!(scroll.offset(), MODAL_MAX_OFFSET);
    }

    #[test]
    fn modal_scroll_resize_clamping_does_not_enable_auto_scroll() {
        let mut scroll = ModalScroll::new_top();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        scroll.scroll(-i32::from(MODAL_HALF_PAGE));
        assert_eq!(scroll.offset(), MODAL_HALF_PAGE);

        scroll.update_dimensions(MODAL_TOTAL, MODAL_TOTAL);
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert_eq!(scroll.offset(), 0);
    }

    const MODAL_CONTENT_W: u16 = 90;
    const MODAL_VIEWPORT_W: u16 = 50;
    const MODAL_MAX_PAN: u16 = MODAL_CONTENT_W - MODAL_VIEWPORT_W;
    const UNCLAIMED: &str = "a modal with nothing off screen must leave the chord alone";

    fn panning_scroll() -> ModalScroll {
        let mut scroll = ModalScroll::new_top();
        scroll.fit_width(MODAL_CONTENT_W, MODAL_VIEWPORT_W);
        scroll
    }

    #[test_case(keybindings::key::PAN_RIGHT.to_key_event(), PAN_STEP as u16 ; "right")]
    #[test_case(keybindings::key::PAN_LEFT.to_key_event(),  0               ; "left_from_the_start")]
    fn the_pan_chord_walks_the_content_sideways(key_event: KeyEvent, expected: u16) {
        let mut scroll = panning_scroll();

        assert!(scroll.handle_key(key_event));
        assert_eq!(scroll.pan(), expected);
    }

    /// Every modal shares `handle_key`, and most of them reflow instead of
    /// running off the edge. Claiming the chord there would take it from the
    /// transcript behind without moving anything.
    #[test_case(keybindings::key::PAN_LEFT.to_key_event()  ; "left")]
    #[test_case(keybindings::key::PAN_RIGHT.to_key_event() ; "right")]
    fn a_modal_that_reflows_never_claims_the_pan_chord(key_event: KeyEvent) {
        let mut scroll = ModalScroll::new_top();
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);

        assert!(!scroll.handle_key(key_event), "{UNCLAIMED}");
        assert_eq!(scroll.pan(), 0);
    }

    #[test]
    fn panning_stops_at_the_widest_line() {
        let mut scroll = panning_scroll();

        scroll.pan_by(i32::from(MODAL_CONTENT_W) * 2);
        assert_eq!(scroll.pan(), MODAL_MAX_PAN);
        scroll.pan_by(-(i32::from(MODAL_CONTENT_W) * 2));
        assert_eq!(scroll.pan(), 0);
    }

    /// A modal redrawn into a wider terminal has less to reach, and a pan left
    /// pointing past the new end would show a blank column.
    #[test]
    fn a_wider_viewport_pulls_the_pan_back() {
        let mut scroll = panning_scroll();
        scroll.pan_by(i32::from(MODAL_MAX_PAN));

        scroll.fit_width(MODAL_CONTENT_W, MODAL_CONTENT_W - 4);
        assert_eq!(scroll.pan(), 4);

        scroll.fit_width(MODAL_CONTENT_W, MODAL_CONTENT_W);
        assert_eq!(scroll.pan(), 0);
    }

    #[test]
    fn reset_forgets_the_pan_along_with_the_offset() {
        let mut scroll = panning_scroll();
        scroll.pan_by(PAN_STEP);
        assert_eq!(scroll.pan(), PAN_STEP as u16);

        scroll.reset();
        assert_eq!(scroll.pan(), 0);
        scroll.fit_width(MODAL_CONTENT_W, MODAL_VIEWPORT_W);
        assert_eq!(
            scroll.pan(),
            0,
            "a reopened modal starts at the left margin"
        );
    }

    const HINTS: [(&str, &str); 2] = [("Enter", "submit"), ("Esc", "close")];
    const HINT_ROW: Rect = Rect::new(4, 9, 40, 1);

    /// The cells one hint occupies, gap excluded. Every pair here is ASCII,
    /// so a byte is a column.
    fn control_width((key, desc): (&str, &str)) -> u16 {
        (key.len() + HINT_KEY_GAP.len() + desc.len()) as u16
    }

    /// The gap before a hint separates two controls and belongs to neither, so
    /// a pointer in it is over nothing.
    #[test]
    fn hint_hits_cover_the_glyphs_and_not_the_gap_before_them() {
        let hits = hint_hits(&HINTS, HINT_ROW);

        assert_eq!(hits.len(), HINTS.len());
        assert_eq!(hits[0].x, HINT_ROW.x + hint_gap_width());
        assert_eq!(hits[0].width, control_width(HINTS[0]));
        assert_eq!(hits[1].x, hits[0].right() + hint_gap_width());
        assert_eq!(hits[1].width, control_width(HINTS[1]));
        assert!(
            hits.iter().all(|hit| hit.y == HINT_ROW.y
                && hit.height == 1
                && hit.right() <= HINT_ROW.right())
        );
    }

    /// The gap still costs the room it takes, so a row measured without it
    /// would offer a hint whose last cells were never drawn.
    #[test]
    fn a_hint_the_row_cannot_hold_whole_gets_no_hit() {
        let width = hint_gap_width() + control_width(HINTS[0]) + hint_gap_width();
        let row = Rect { width, ..HINT_ROW };

        assert_eq!(hint_hits(&HINTS, row).len(), 1);
    }

    #[test]
    fn a_hovered_hint_marks_its_glyphs_and_leaves_the_gap_plain() {
        let line = hint_line_hovered(&HINTS, Some(1));
        let marked: Vec<&str> = line
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.as_ref())
            .collect();

        let description = format!("{HINT_KEY_GAP}{}", HINTS[1].1);
        assert_eq!(marked, [HINTS[1].0, description.as_str()]);
    }
}
