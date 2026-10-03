pub(crate) mod code_view;
pub mod command;
pub(crate) mod command_modal;
pub(crate) mod command_text;
pub(crate) mod commit_popup;
pub(crate) mod completion;
pub(crate) mod context_modal;
pub(crate) mod decisions_modal;
pub(crate) mod docs_modal;
pub(crate) mod document_view;
pub(crate) mod environment_card;
pub(crate) mod file_picker;
pub(crate) mod file_walk;
pub(crate) mod form;
pub(crate) mod goal_modal;
pub(crate) mod help_modal;
pub mod input;
pub(crate) mod json_text;
pub mod keybindings;
pub(crate) mod list_picker;
pub(crate) mod login_picker;
pub(crate) mod logs_modal;
pub(crate) mod lua_float;
pub(crate) mod mcp_picker;
pub(crate) mod memory_card;
pub(crate) mod memory_picker;
pub(crate) mod mention_popup;
pub(crate) mod message_actions;
pub mod messages;
pub(crate) mod modal;
pub(crate) mod mode_submission;
pub(crate) mod model_picker;
pub(crate) mod paste_editor;
pub(crate) mod peer_card;
pub(crate) mod peer_manager;
pub(crate) mod permission_prompt;
pub(crate) mod permission_scope;
pub(crate) mod permissions_picker;
pub(crate) mod plan_form;
pub(crate) mod progress_bar;
pub(crate) mod projection_modal;
pub(crate) mod prompt_profile_picker;
pub(crate) mod prompt_progress;
pub(crate) mod question_form;
pub(crate) mod queue_actions;
pub mod queue_panel;
pub(crate) mod review;
pub(crate) mod rewind_picker;
pub(crate) mod sandbox_manager;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub(crate) mod section_tabs;
pub(crate) mod session_picker;
pub(crate) mod session_relocation;
pub(crate) mod shell_modal;
pub(crate) mod skills_modal;
pub(crate) mod split_layout;
pub(crate) mod stash_picker;
pub mod status_bar;
pub(crate) mod storage_modal;
pub(crate) mod stream_modal;
pub(crate) mod streaming_content;
pub(crate) mod system_prompt_modal;
pub(crate) mod tab_bar;
pub(crate) mod task_card;
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
pub(crate) mod worktree_picker;

use std::iter;
use std::mem;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::AgentInput;
use caudra_agent::peers::{PeerDecision, PeerReviewToken};
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::tools::{SHELL_TOOL_NAME, ToolEffect};
use caudra_agent::worktree::Request as WorktreeRequest;
use caudra_agent::{
    BufferSnapshot, CallStage, ImageSource, SubagentActivity, SubagentProgress, ToolInput,
    ToolOutput,
};
use caudra_config::InboundPolicy;
use caudra_providers::model_registry::Binding;
use caudra_providers::{CaudraId, HistoryItem, ModelPurpose, PeerMessageOrigin, TaskEventOrigin};
use caudra_storage::sessions::SessionRelocation;
use caudra_workbench::text_field::FieldStyles;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::animation::live_elapsed;
use crate::selection::wrap_breaks;
use keybindings::Bind;
use modal::FooterHits;
use worktree_picker::WorktreeView;

pub(crate) const CHEVRON: &str = "❯ ";
/// Selected text in every field, laid over whatever colour it already carries.
pub(crate) const SELECTION: Style = Style::new().add_modifier(Modifier::REVERSED);
const DIGIT_GROUP: usize = 3;
/// Columns a modal pans per key press. Roughly one column of a token table, so
/// a reader walks the table a field at a time rather than a glyph at a time.
const PAN_STEP: i32 = 8;
const BYTE_STEP: u64 = 1024;
const BYTE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
const IEC_BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

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

/// How a text field is painted: `text` for what was typed, the selection
/// reversed, and the caret in the theme's cursor colours.
pub(crate) fn field_styles(text: Style) -> FieldStyles {
    let theme = crate::theme::current();
    FieldStyles {
        text,
        selection: SELECTION,
        caret: theme.cursor,
        placeholder: theme.input_placeholder,
    }
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

/// One entry of a hint bar: what it says, and the key a click on it presses.
/// The key travels with the label, so a bar can never advertise a control
/// that does something other than what it says, or nothing at all.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hint {
    label: &'static str,
    description: &'static str,
    /// `None` is a group hint such as `↑↓ select`: drawn, never hovered,
    /// never pressed.
    press: Option<KeyEvent>,
}

impl Hint {
    pub(crate) const fn bind(bind: Bind, description: &'static str) -> Self {
        Self {
            label: bind.label,
            description,
            press: Some(bind.to_key_event()),
        }
    }

    pub(crate) const fn key(label: &'static str, code: KeyCode, description: &'static str) -> Self {
        Self {
            label,
            description,
            press: Some(KeyEvent::new(code, KeyModifiers::NONE)),
        }
    }

    /// A single printable glyph that is its own key, like `y` or `/`.
    pub(crate) const fn char(label: &'static str, description: &'static str) -> Self {
        Self::key(
            label,
            KeyCode::Char(label.as_bytes()[0] as char),
            description,
        )
    }

    pub(crate) const fn inert(label: &'static str, description: &'static str) -> Self {
        Self {
            label,
            description,
            press: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn label(&self) -> &'static str {
        self.label
    }

    #[cfg(test)]
    pub(crate) fn description(&self) -> &'static str {
        self.description
    }

    pub(crate) fn press(&self) -> Option<KeyEvent> {
        self.press
    }
}

/// One hint's spans and the cells they occupy, the gap before it excluded.
/// Drawing, hit testing and hover all read this, so a hint cannot be styled in
/// one place and measured in another, and a hit rect cannot drift off the
/// glyphs it claims to cover.
fn hint_parts(hints: &[Hint]) -> Vec<(Vec<Span<'static>>, u16)> {
    let t = crate::theme::current();
    hints
        .iter()
        .map(|hint| {
            let mut spans = Vec::new();
            for (i, part) in hint.label.split('/').enumerate() {
                if i > 0 {
                    spans.push(Span::styled("/", t.tool_dim));
                }
                spans.push(Span::styled(part.to_string(), t.keybind_key));
            }
            spans.push(Span::styled(
                format!("{HINT_KEY_GAP}{}", hint.description),
                t.tool_dim,
            ));
            let width = UnicodeWidthStr::width(hint.label)
                + UnicodeWidthStr::width(HINT_KEY_GAP)
                + UnicodeWidthStr::width(hint.description);
            (spans, width as u16)
        })
        .collect()
}

fn hint_gap_width() -> u16 {
    UnicodeWidthStr::width(HINT_GAP) as u16
}

pub(crate) fn hint_line(hints: &[Hint]) -> Line<'static> {
    hint_line_hovered(hints, None)
}

/// The hint bar with one hint marked. A hovered hint reverses whole, key and
/// description together, so the pointer marks the control rather than half of
/// it. The gap before a hint separates two controls and belongs to neither, so
/// it is drawn plain however the pointer moves.
pub(crate) fn hint_line_hovered(hints: &[Hint], hovered: Option<usize>) -> Line<'static> {
    let spans = hint_parts(hints)
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

/// Where each hint landed inside `area`, so a click can name the one it hit.
/// The rect covers the hint's own glyphs and not the gap that precedes it, so
/// the pointer acts on a control only once it is over one. Hints that run
/// past the right edge are dropped rather than clipped: a hint the reader
/// cannot fully see is not one they can knowingly press.
pub(crate) fn hint_hits(hints: &[Hint], area: Rect) -> Vec<Rect> {
    let mut x = area.x;
    let mut hits = Vec::with_capacity(hints.len());
    for (_, width) in hint_parts(hints) {
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

/// A hint bar that answers the pointer: the hints it drew, where each landed,
/// and which one a press is committed to. A click stands in for the key the
/// hint names, so the bar's owner feeds it to its own key handler and the two
/// paths can never drift.
#[derive(Default)]
pub(crate) struct HintBar {
    hints: Vec<Hint>,
    hits: FooterHits,
}

impl HintBar {
    /// The bar as it should be drawn in `area`, with the hint under the
    /// pointer marked. Recording the geometry here is what makes the next
    /// click land on what was drawn rather than on what was drawn before.
    ///
    /// A hint that does not fit is dropped rather than drawn and clipped
    /// mid-glyph, so the bar advertises exactly the controls [`hint_hits`]
    /// says a pointer can reach.
    pub(crate) fn line(&mut self, area: Rect, hints: Vec<Hint>) -> Line<'static> {
        let hits = hint_hits(&hints, area);
        self.hints = hints;
        self.hints.truncate(hits.len());
        self.hits.set(hits);
        hint_line_hovered(&self.hints, self.hovered())
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect, hints: Vec<Hint>) {
        let line = self.line(area, hints);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The key of the hint a press and release both landed on. A group hint
    /// yields nothing, however precisely it was clicked.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<KeyEvent> {
        self.hits
            .handle_mouse(event)
            .and_then(|index| self.hints.get(index)?.press)
    }

    pub(crate) fn reset(&mut self) {
        self.hints.clear();
        self.hits.reset();
    }

    /// The pressable hint under the pointer. A docked owner asks this to keep
    /// a press on its bar from falling through to whatever is drawn behind.
    pub(crate) fn hovered(&self) -> Option<usize> {
        self.hits
            .hovered()
            .filter(|&index| self.hints[index].press.is_some())
    }
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
    hanging_spans(prefix, vec![text], width)
}

/// [`hanging_lines`] for text in more than one style, such as a coloured
/// command. Each row keeps the styles of the characters it took.
pub(crate) fn hanging_spans(
    prefix: Span<'static>,
    spans: Vec<Span<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    let indent = UnicodeWidthStr::width(prefix.content.as_ref()) as u16;
    let styled: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(|character| (character, span.style))
        })
        .collect();
    let chars: Vec<char> = styled.iter().map(|(character, _)| *character).collect();
    let first_style = spans.first().map(|span| span.style).unwrap_or_default();
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
            let mut end = starts.get(row + 1).copied().unwrap_or(chars.len());
            // Trailing spaces are what the wrap broke on; keeping them could
            // push a row past the width it was measured for.
            if end != chars.len() {
                while end > start && chars[end - 1].is_whitespace() {
                    end -= 1;
                }
            }
            let mut line = vec![match row {
                0 => prefix.clone(),
                _ => hang.clone(),
            }];
            for run in styled[start..end].chunk_by(|left, right| left.1 == right.1) {
                line.push(Span::styled(
                    run.iter()
                        .map(|(character, _)| character)
                        .collect::<String>(),
                    run[0].1,
                ));
            }
            if line.len() == 1 {
                line.push(Span::styled(String::new(), first_style));
            }
            Line::from(line)
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
        style.add_modifier(Modifier::REVERSED)
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

/// `512 B`, `2.0 KB`: a size in the largest unit it fills.
pub(crate) fn format_bytes(bytes: u64) -> String {
    scaled_bytes(bytes, &BYTE_UNITS)
}

/// [`format_bytes`] under the IEC names, `2.0 KiB`, for the storage report.
pub(crate) fn format_iec_bytes(bytes: u64) -> String {
    scaled_bytes(bytes, &IEC_BYTE_UNITS)
}

fn scaled_bytes(bytes: u64, units: &[&str; 5]) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= BYTE_STEP as f64 && unit + 1 < units.len() {
        value /= BYTE_STEP as f64;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", units[0])
    } else {
        format!("{value:.1} {}", units[unit])
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

/// The character a key types with neither Ctrl nor Alt held, so a letter
/// command never swallows a chord spelled with the same letter.
pub(crate) fn plain_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(character)
        }
        _ => None,
    }
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

    /// [`Self::reveal`] for a view the selection leads. Settling above the
    /// bottom stops following the tail, so the next [`Self::update_dimensions`]
    /// keeps the range in view instead of snapping back to the end. A live view
    /// calls `reveal`, which moves to its cursor without giving its tail up.
    pub fn reveal_and_hold(&mut self, top: u16, height: u16) {
        self.reveal(top, height);
        self.repin();
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

pub struct PlanHandoff {
    pub(crate) input: AgentInput,
    pub(crate) content: String,
    pub(crate) source: String,
    pub(crate) header: String,
    pub(crate) target: PlanTarget,
}

pub enum Action {
    SendMessage(Box<AgentInput>),
    ClearAndImplement(Box<PlanHandoff>),
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
    ListPeers,
    PeerMessages(String),
    RefreshPeers,
    ReviewPeerMessage(String),
    DecidePeerMessage {
        token: PeerReviewToken,
        decision: PeerDecision,
    },
    SetPeerInbound(InboundPolicy),
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
    OpenWorktrees(WorktreeView),
    /// Opens the checkout at the path, where this repository's other
    /// checkouts are.
    OpenWorktree(PathBuf),
    InspectWorktreeRemoval(PathBuf),
    RunWorktree(WorktreeRequest),
    ChangeRemoteWorkingDirectory(caudra_workspace::DirectoryNavigation),
    RemoteControl(String),
    ChangeModel(String),
    CompleteProviderSetup(String),
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
    /// Opens a session that works in `cwd`, in another checkout of this
    /// repository.
    OpenSessionElsewhere {
        id: caudra_storage::id::CaudraId,
        cwd: PathBuf,
    },
    DeleteSession(caudra_storage::id::CaudraId),
    SetSessionTitle {
        id: caudra_storage::id::CaudraId,
        title: String,
    },
    GenerateSessionTitle(caudra_storage::id::CaudraId),
    OpenUrl(String),
    Btw(String),
    Extract,
}

pub struct ForkedSession {
    pub session: crate::AppSession,
    pub lease: Arc<caudra_storage::sessions::SessionLease>,
    pub draft: Option<ForkDraft>,
    pub warning: Option<String>,
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
    history: Vec<SubagentActivity>,
    current_activity: Option<usize>,
}

impl ToolProgress {
    pub fn live(report: SubagentProgress) -> Self {
        let history = if report.activity.children().is_empty() {
            Vec::new()
        } else {
            vec![report.activity.clone()]
        };
        let current_activity = (!history.is_empty()).then_some(0);
        Self {
            report,
            since: Some(Instant::now()),
            history,
            current_activity,
        }
    }

    pub fn update(&mut self, mut report: SubagentProgress) {
        if self.has_history() || !report.activity.children().is_empty() {
            let existing = match &report.activity {
                SubagentActivity::Tool { call_id: Some(id), .. } => {
                    self.history.iter().position(|activity| {
                        matches!(activity, SubagentActivity::Tool { call_id: Some(other), .. } if other == id)
                    })
                }
                SubagentActivity::Tool { call_id: None, name, .. } => {
                    self.current_activity.filter(|&index| {
                        self.report.tools == report.tools
                            && matches!(
                                &self.history[index],
                                SubagentActivity::Tool { call_id: None, name: other, .. } if other == name
                            )
                    })
                }
                activity => self.current_activity.filter(|&index| {
                    mem::discriminant(&self.history[index]) == mem::discriminant(activity)
                }),
            };
            self.current_activity = Some(if let Some(index) = existing {
                if let SubagentActivity::Tool { children, .. } = &mut report.activity
                    && children.is_empty()
                {
                    *children = self.history[index].children().to_vec();
                }
                self.history[index] = report.activity.clone();
                index
            } else {
                self.history.push(report.activity.clone());
                self.history.len() - 1
            });
        }
        self.report = report;
        self.since = Some(Instant::now());
    }

    pub fn activities(&self) -> impl Iterator<Item = (&SubagentActivity, bool)> {
        self.history
            .iter()
            .enumerate()
            .map(move |(index, activity)| (activity, self.current_activity == Some(index)))
            .chain(
                self.current_activity
                    .is_none()
                    .then_some((&self.report.activity, true)),
            )
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn is_live(&self) -> bool {
        self.since.is_some()
    }

    pub fn settle(&mut self) {
        self.report.elapsed = self.elapsed();
        self.since = None;
        self.history = Vec::new();
        self.current_activity = None;
    }

    pub fn elapsed(&self) -> Duration {
        self.report.elapsed + self.since.map_or(Duration::ZERO, live_elapsed)
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
    /// Where the call is before it runs, which its title names in place of
    /// its verb. Live only, like [`Self::live_body`]: a restored call has long
    /// since run.
    pub tool_stage: Option<CallStage>,
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
    pub(crate) fn peer(text: &str, origin: PeerMessageOrigin) -> Self {
        let safe_body = text
            .lines()
            .map(|line| {
                line.chars()
                    .flat_map(char::escape_debug)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!(
            "Peer: {:?}\nMessage: {:?}\nSender session: {:?}\nReply target: {:?}\n\n{safe_body}",
            origin.sender_name, origin.message_id, origin.sender_session_id, origin.reply_target,
        );
        Self::new(DisplayRole::PeerMessage(Box::new(origin)), text)
    }

    pub(crate) fn injected(text: String, task_event: Option<TaskEventOrigin>) -> Self {
        let role = task_event.map_or(DisplayRole::Injected, |origin| {
            DisplayRole::TaskDelivery(Box::new(origin))
        });
        Self::new(role, text)
    }

    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            tool_preview_pending: false,
            tool_stage: None,
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
            tool_stage: None,
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
    TaskDelivery(Box<TaskEventOrigin>),
    PeerMessage(Box<PeerMessageOrigin>),
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
        tiers: ModelPricing::UNTIERED,
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
        supports_cache_breakpoints_override: None,
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
mod tool_progress_tests {
    use super::ToolProgress;
    use super::tool_display::progress_lines;
    use caudra_agent::tools::{BATCH_TOOL_NAME, SHELL_TOOL_NAME};
    use caudra_agent::{ActivityChild, BatchToolStatus, SubagentActivity, SubagentProgress};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use test_case::test_case;

    const FIRST_CALL: &str = "batch-1";
    const SECOND_CALL: &str = "batch-2";
    const THIRD_CALL: &str = "tool-3";
    const BATCH_SUMMARY: &str = "inspect files";
    const CHILD_SUMMARY: &str = "ls";
    const TOOL_PREVIEW: &str = "inspect another file";
    const THINKING_TITLE: &str = "Inspecting files";
    const UPDATED_THINKING_TITLE: &str = "Inspecting more files";
    const PROGRESS_WIDTH: u16 = 80;
    const TOOL_COUNT: u32 = 8;
    const REPORTED_ELAPSED: Duration = Duration::from_secs(5);
    const STALE_ANCHOR_AGE: Duration = Duration::from_secs(60);

    fn report(activity: SubagentActivity) -> SubagentProgress {
        SubagentProgress {
            activity,
            tools: TOOL_COUNT,
            elapsed: REPORTED_ELAPSED,
        }
    }

    fn batch_report(id: Option<&str>, statuses: &[BatchToolStatus]) -> SubagentProgress {
        let activity = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            BATCH_SUMMARY,
            statuses
                .iter()
                .map(|&status| ActivityChild {
                    tool: Arc::from(SHELL_TOOL_NAME),
                    summary: CHILD_SUMMARY.into(),
                    status,
                })
                .collect(),
        );
        report(match id {
            Some(id) => activity.with_call_id(id),
            None => activity,
        })
    }

    #[test_case(BatchToolStatus::Running ; "unfinished")]
    #[test_case(BatchToolStatus::Error ; "failed")]
    fn identical_consecutive_keyed_calls_remain_distinct(status: BatchToolStatus) {
        let first = batch_report(Some(FIRST_CALL), &[status]);
        let second = batch_report(Some(SECOND_CALL), &[status]);
        let mut progress = ToolProgress::live(first.clone());
        assert!(progress.has_history());
        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [(&first.activity, true)]
        );

        progress.update(second.clone());
        progress.update(second.clone());

        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [(&first.activity, false), (&second.activity, true)]
        );
    }

    #[test_case(false ; "active_batch")]
    #[test_case(true ; "earlier_batch")]
    fn per_call_updates_replace_rows_without_appending_snapshots(interrupted: bool) {
        let first = batch_report(
            Some(FIRST_CALL),
            &[BatchToolStatus::Pending, BatchToolStatus::Running],
        );
        let second = batch_report(Some(SECOND_CALL), &[BatchToolStatus::Running]);
        let mut progress = ToolProgress::live(first);
        if interrupted {
            progress.update(second.clone());
        }

        for status in [
            BatchToolStatus::Running,
            BatchToolStatus::Success,
            BatchToolStatus::Error,
        ] {
            let updated = batch_report(Some(FIRST_CALL), &[status, BatchToolStatus::Running]);
            progress.update(updated.clone());
            let mut expected = vec![(&updated.activity, true)];
            if interrupted {
                expected.push((&second.activity, false));
            }
            assert_eq!(progress.activities().collect::<Vec<_>>(), expected);
        }
    }

    #[test_case(FIRST_CALL ; "same_call_after_pending")]
    #[test_case(SECOND_CALL ; "identical_independent_call_after_pending")]
    fn pending_phases_preserve_keyed_batch_identity(next_id: &str) {
        let first = batch_report(Some(FIRST_CALL), &[BatchToolStatus::Running]);
        let mut progress = ToolProgress::live(first.clone());
        let pending = SubagentActivity::tool(Arc::from(BATCH_TOOL_NAME), "").with_call_id(next_id);
        progress.update(report(pending.clone()));
        if next_id == FIRST_CALL {
            let retained = SubagentActivity::batch(
                Arc::from(BATCH_TOOL_NAME),
                "",
                first.activity.children().to_vec(),
            )
            .with_call_id(next_id);
            assert_eq!(
                progress.activities().collect::<Vec<_>>(),
                [(&retained, true)]
            );
            assert_eq!(
                progress.report.activity.children(),
                first.activity.children()
            );
        } else {
            assert_eq!(
                progress.activities().collect::<Vec<_>>(),
                [(&first.activity, false), (&pending, true)]
            );
        }

        let next = batch_report(Some(next_id), &[BatchToolStatus::Running]);
        progress.update(next.clone());
        let mut expected = Vec::new();
        if next_id != FIRST_CALL {
            expected.push((&first.activity, false));
        }
        expected.push((&next.activity, true));
        assert_eq!(progress.activities().collect::<Vec<_>>(), expected);
    }

    #[test_case(SubagentActivity::Thinking { title: None } ; "thinking")]
    #[test_case(SubagentActivity::Responding ; "responding")]
    #[test_case(SubagentActivity::Compacting ; "compacting")]
    #[test_case(SubagentActivity::Retrying ; "retrying")]
    #[test_case(SubagentActivity::AwaitingPermission ; "permission")]
    #[test_case(SubagentActivity::tool(Arc::from(SHELL_TOOL_NAME), CHILD_SUMMARY) ; "single_tool")]
    fn phases_keep_the_last_observed_batch_statuses(phase: SubagentActivity) {
        let batch = batch_report(
            Some(FIRST_CALL),
            &[
                BatchToolStatus::Pending,
                BatchToolStatus::Running,
                BatchToolStatus::Success,
                BatchToolStatus::Error,
            ],
        );
        let mut progress = ToolProgress::live(batch.clone());
        progress.update(report(phase.clone()));
        progress.update(report(phase.clone()));

        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [(&batch.activity, false), (&phase, true)]
        );
        let responding = SubagentActivity::Responding;
        progress.update(report(responding.clone()));
        let mut expected = vec![(&batch.activity, false)];
        if phase != responding {
            expected.push((&phase, false));
        }
        expected.push((&responding, true));
        assert_eq!(progress.activities().collect::<Vec<_>>(), expected);
    }

    #[test_case(1 ; "single_child")]
    #[test_case(5 ; "multiple_children")]
    fn intervening_phases_do_not_shrink_rendered_history(children: usize) {
        let mut statuses = vec![BatchToolStatus::Running; children];
        let mut progress = ToolProgress::live(batch_report(Some(FIRST_CALL), &statuses));
        let permission = SubagentActivity::AwaitingPermission;
        let thinking = SubagentActivity::Thinking {
            title: Some(THINKING_TITLE.into()),
        };
        statuses[0] = BatchToolStatus::Error;
        let updated = batch_report(Some(FIRST_CALL), &statuses);
        let preview = SubagentActivity::tool(Arc::from(BATCH_TOOL_NAME), TOOL_PREVIEW)
            .with_call_id(FIRST_CALL);
        let mut heights = vec![progress_lines(&progress, "", PROGRESS_WIDTH).len()];

        for incoming in [
            report(permission.clone()),
            updated.clone(),
            report(thinking.clone()),
            report(preview),
        ] {
            progress.update(incoming);
            heights.push(progress_lines(&progress, "", PROGRESS_WIDTH).len());
            assert_eq!(
                progress
                    .activities()
                    .filter(|(_, current)| *current)
                    .count(),
                1
            );
        }

        assert_eq!(
            heights,
            [
                children + 1,
                children + 2,
                children + 2,
                children + 3,
                children + 3
            ]
        );
        let retained = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            TOOL_PREVIEW,
            updated.activity.children().to_vec(),
        )
        .with_call_id(FIRST_CALL);
        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [(&retained, true), (&permission, false), (&thinking, false)]
        );
        assert_eq!(
            progress.report.activity.children(),
            updated.activity.children()
        );
    }

    #[test_case(false ; "before_any_batch")]
    #[test_case(true ; "after_first_batch")]
    fn thinking_title_updates_reuse_the_current_phase(retained: bool) {
        let batch = batch_report(Some(FIRST_CALL), &[BatchToolStatus::Running]);
        let mut progress = ToolProgress::live(if retained {
            batch.clone()
        } else {
            report(SubagentActivity::Responding)
        });

        for title in [None, Some(THINKING_TITLE), Some(UPDATED_THINKING_TITLE)] {
            let thinking = SubagentActivity::Thinking {
                title: title.map(str::to_owned),
            };
            progress.update(report(thinking.clone()));
            let mut expected = Vec::new();
            if retained {
                expected.push((&batch.activity, false));
            }
            expected.push((&thinking, true));
            assert_eq!(progress.activities().collect::<Vec<_>>(), expected);
            assert_eq!(progress.has_history(), retained);
        }
    }

    #[test_case(SHELL_TOOL_NAME ; "ordinary_tool")]
    #[test_case(BATCH_TOOL_NAME ; "pending_batch")]
    fn keyed_tool_previews_update_their_original_record(tool: &str) {
        let batch = batch_report(Some(FIRST_CALL), &[BatchToolStatus::Running]);
        let mut progress = ToolProgress::live(batch.clone());
        let first =
            SubagentActivity::tool(Arc::from(tool), CHILD_SUMMARY).with_call_id(SECOND_CALL);
        let second =
            SubagentActivity::tool(Arc::from(tool), CHILD_SUMMARY).with_call_id(THIRD_CALL);
        let thinking = SubagentActivity::Thinking { title: None };
        progress.update(report(first));
        progress.update(report(thinking.clone()));
        progress.update(report(second.clone()));
        let preview =
            SubagentActivity::tool(Arc::from(tool), TOOL_PREVIEW).with_call_id(SECOND_CALL);
        progress.update(report(preview.clone()));
        progress.update(report(preview.clone()));

        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [
                (&batch.activity, false),
                (&preview, true),
                (&thinking, false),
                (&second, false)
            ]
        );
    }

    #[test_case(&[1, 5, 2] ; "small_large_small")]
    fn successive_batches_survive_thinking_phases(sizes: &[usize]) {
        let thinking = SubagentActivity::Thinking { title: None };
        let mut progress = ToolProgress::live(report(thinking.clone()));
        assert!(!progress.has_history());

        for (index, &size) in sizes.iter().enumerate() {
            progress.update(batch_report(
                Some(&index.to_string()),
                &vec![BatchToolStatus::Running; size],
            ));
            assert_eq!(progress.activities().count(), index * 2 + 1);
            progress.update(report(thinking.clone()));
            assert_eq!(
                progress
                    .activities()
                    .map(|(activity, current)| (activity.children().len(), current))
                    .collect::<Vec<_>>(),
                sizes[..=index]
                    .iter()
                    .enumerate()
                    .flat_map(|(batch, &size)| [(size, false), (0, batch == index)])
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test_case(false ; "separate_instances")]
    #[test_case(true ; "cloned_instance")]
    fn progress_histories_are_independent(cloned: bool) {
        let batch = batch_report(Some(FIRST_CALL), &[BatchToolStatus::Running]);
        let mut first = ToolProgress::live(batch.clone());
        let second = if cloned {
            first.clone()
        } else {
            ToolProgress::live(batch.clone())
        };
        first.update(batch_report(Some(FIRST_CALL), &[BatchToolStatus::Success]));
        first.update(batch_report(Some(SECOND_CALL), &[BatchToolStatus::Error]));
        first.settle();

        assert!(!first.has_history());
        assert!(!first.is_live());
        assert!(second.has_history());
        assert!(second.is_live());
        assert_eq!(
            second.activities().collect::<Vec<_>>(),
            [(&batch.activity, true)]
        );
    }

    #[test_case(false ; "phase_boundary")]
    #[test_case(true ; "tool_tally_boundary")]
    fn anonymous_batches_merge_only_within_one_uninterrupted_call(tally_boundary: bool) {
        let batch = batch_report(None, &[BatchToolStatus::Running]);
        let mut progress = ToolProgress::live(batch.clone());
        progress.update(batch.clone());
        assert_eq!(progress.activities().count(), 1);

        let mut next = batch.clone();
        if tally_boundary {
            next.tools += 1;
        } else {
            progress.update(report(SubagentActivity::Thinking { title: None }));
        }
        progress.update(next);
        let thinking = SubagentActivity::Thinking { title: None };
        let mut expected = vec![(&batch.activity, false)];
        if !tally_boundary {
            expected.push((&thinking, false));
        }
        expected.push((&batch.activity, true));
        assert_eq!(progress.activities().collect::<Vec<_>>(), expected);
    }

    #[test_case(64 ; "many_batches")]
    fn all_batches_remain_available_for_the_active_task(calls: usize) {
        let mut progress = ToolProgress::live(report(SubagentActivity::Responding));
        for index in 0..calls {
            progress.update(batch_report(
                Some(&index.to_string()),
                &[BatchToolStatus::Running],
            ));
        }
        let ids: Vec<_> = progress
            .activities()
            .filter_map(|(activity, current)| match activity {
                SubagentActivity::Tool { call_id, .. } => Some((call_id.clone(), current)),
                _ => None,
            })
            .collect();

        assert_eq!(
            ids,
            (0..calls)
                .map(|index| (Some(index.to_string()), index == calls - 1))
                .collect::<Vec<_>>()
        );
    }

    #[test_case(SubagentActivity::Responding ; "non_batch")]
    #[test_case(batch_report(Some(SECOND_CALL), &[BatchToolStatus::Error]).activity ; "batch")]
    fn updating_resets_the_elapsed_anchor_and_settling_keeps_the_report(
        activity: SubagentActivity,
    ) {
        let mut progress =
            ToolProgress::live(batch_report(Some(FIRST_CALL), &[BatchToolStatus::Running]));
        progress.since = Some(Instant::now() - STALE_ANCHOR_AGE);
        let before_update = Instant::now();
        let latest = report(activity);
        progress.update(latest.clone());

        assert!(progress.since.is_some_and(|since| since >= before_update));
        assert_eq!(progress.report, latest);
        progress.settle();
        assert!(!progress.is_live());
        assert!(!progress.has_history());
        assert_eq!(
            progress.activities().collect::<Vec<_>>(),
            [(&latest.activity, true)]
        );
        assert_eq!(progress.report.tools, latest.tools);
        assert_eq!(progress.elapsed(), progress.report.elapsed);
        assert_eq!(progress.history.capacity(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::keybindings::key;
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

    #[test_case(0,                      "0 B",    "0 B"     ; "zero")]
    #[test_case(512,                    "512 B",  "512 B"   ; "bytes")]
    #[test_case(2048,                   "2.0 KB", "2.0 KiB" ; "kilobytes")]
    #[test_case(5 * 1024 * 1024,        "5.0 MB", "5.0 MiB" ; "megabytes")]
    #[test_case(7 * 1024 * 1024 * 1024, "7.0 GB", "7.0 GiB" ; "gigabytes")]
    fn sizes_read_in_the_largest_unit_they_fill(bytes: u64, short: &str, iec: &str) {
        assert_eq!(format_bytes(bytes), short);
        assert_eq!(format_iec_bytes(bytes), iec);
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

    /// `End` pins the view to its tail, and a selection moved to the top after
    /// it has to stay on screen past the next redraw rather than snap back.
    #[test_case(ModalScroll::new()     ; "bottom_default")]
    #[test_case(ModalScroll::new_top() ; "top_default")]
    fn a_held_reveal_survives_the_next_redraw(mut scroll: ModalScroll) {
        scroll.update_dimensions(MODAL_TOTAL, MODAL_VIEWPORT);
        assert!(scroll.handle_key(keybindings::key::DOC_BOTTOM.to_key_event()));

        scroll.reveal_and_hold(0, 1);
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

    const HINTS: [Hint; 2] = [
        Hint::bind(key::ENTER, "submit"),
        Hint::bind(key::ESC, "close"),
    ];
    const HINT_ROW: Rect = Rect::new(4, 9, 40, 1);
    const GROUP_HINT_ACTED: &str = "a group hint names no key, so a click on it must do nothing";
    const CLIPPED_HINT: &str = "a hint the row cannot hold whole must not be drawn at all";

    /// The cells one hint occupies, gap excluded. Every hint here is ASCII,
    /// so a byte is a column.
    fn control_width(hint: Hint) -> u16 {
        (hint.label.len() + HINT_KEY_GAP.len() + hint.description.len()) as u16
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

    /// Drawing the full list let the widget clip the overflow mid-glyph while
    /// [`hint_hits`] had already dropped it, so the row advertised a control
    /// no pointer could reach.
    #[test]
    fn a_bar_drops_the_hint_it_cannot_hit_instead_of_clipping_it() {
        let width = hint_gap_width() + control_width(HINTS[0]) + hint_gap_width();
        let row = Rect { width, ..HINT_ROW };
        let mut bar = HintBar::default();

        let line = bar.line(row, HINTS.to_vec());
        let drawn: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(drawn.contains(HINTS[0].description));
        assert!(!drawn.contains(HINTS[1].description), "{CLIPPED_HINT}");
        assert!(line.width() <= usize::from(row.width), "{CLIPPED_HINT}");
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

        let description = format!("{HINT_KEY_GAP}{}", HINTS[1].description);
        assert_eq!(marked, [HINTS[1].label, description.as_str()]);
    }

    fn bar_mouse(kind: crossterm::event::MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A click on a hint is the key it names, and a group hint is only text:
    /// it neither marks itself under the pointer nor answers a press.
    #[test]
    fn a_hint_bar_click_presses_the_key_and_a_group_hint_is_text() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut bar = HintBar::default();
        let hints = vec![Hint::inert("↑↓", "select"), HINTS[1]];
        bar.line(HINT_ROW, hints.clone());
        let [group, close] = [bar.hits.hit(0), bar.hits.hit(1)];

        bar.handle_mouse(bar_mouse(MouseEventKind::Moved, group));
        assert_eq!(bar.hovered(), None, "{GROUP_HINT_ACTED}");
        bar.handle_mouse(bar_mouse(MouseEventKind::Down(MouseButton::Left), group));
        assert_eq!(
            bar.handle_mouse(bar_mouse(MouseEventKind::Up(MouseButton::Left), group)),
            None,
            "{GROUP_HINT_ACTED}"
        );

        bar.handle_mouse(bar_mouse(MouseEventKind::Moved, close));
        assert_eq!(bar.hovered(), Some(1));
        bar.handle_mouse(bar_mouse(MouseEventKind::Down(MouseButton::Left), close));
        assert_eq!(
            bar.handle_mouse(bar_mouse(MouseEventKind::Up(MouseButton::Left), close)),
            Some(key::ESC.to_key_event())
        );
    }
}
