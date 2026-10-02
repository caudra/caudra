use caudra_agent::context::{
    ContextBuiltinState, ContextBuiltinTool, ContextMcpStatus, ContextSnapshot,
};
use caudra_agent::format_settled_duration;
use caudra_agent::tools::TOOL_SEARCH_TOOL_NAME as TOOL_SEARCH;
use caudra_agent::tools::report::{CATALOG_SOURCE, REASON_CATALOG};
use caudra_grab::grab_scope;
use caudra_providers::{format_tokens_u64, token_label};
use caudra_storage::tool_ledger::{ToolOutcome, ToolSlice, ToolStats};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::time::Duration;

use crate::components::keybindings::key;
use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, Overlay, bar_area, escape_terminal_controls};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Tools ";
const SESSION_TITLE: &str = " Tools - session ";
const PROJECT_TITLE: &str = " Tools - project ";
const GLOBAL_TITLE: &str = " Tools - global ";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 82;
const H_PAD: u16 = 2;
const H_PAD_STEP_WIDTH: u16 = 16;
const DECLARED_GLYPH: &str = "\u{25cf}";
const DEFERRED_GLYPH: &str = "\u{25cb}";
const DISABLED_GLYPH: &str = "\u{d7}";
const NO_SNAPSHOT: &str = "No tool report is available yet.";
const NO_SNAPSHOT_HINT: &str = "A report appears when a request is prepared.";
const NO_MCP: &str = "No MCP tools.";
/// An empty ledger, told apart from one that could not be read.
const NO_ACTIVITY: &str = "No tool calls recorded yet.";
const NO_ACTIVITY_HINT: &str = "Recording began with this release, so earlier work is not counted.";
const STATS_UNAVAILABLE: &str = "Recorded tool activity is unavailable.";
const FAILURES_HEADING: &str = "Failures";
/// Every token figure is estimated with o200k, exact only for OpenAI models.
const ESTIMATE_MARKER: &str = "~";
const TOOL_COL_MIN: usize = 18;
const COUNT_COL: usize = 7;
const RATE_COL: usize = 6;
const TOKENS_COL: usize = 8;
const TIME_COL: usize = 9;
const COL_GAP: usize = 2;
const PERCENT: f64 = 100.0;
/// A share with nothing to divide by, told apart from a share of zero.
const NO_SHARE: &str = "\u{2014}";

/// Which answer the modal is showing: what the model can call, what this
/// session called, what this project has called, or everything ever recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolsScope {
    #[default]
    Inventory,
    Session,
    Project,
    Global,
}

impl ToolsScope {
    fn next(self) -> Self {
        match self {
            Self::Inventory => Self::Session,
            Self::Session => Self::Project,
            Self::Project => Self::Global,
            Self::Global => Self::Inventory,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Inventory => TITLE,
            Self::Session => SESSION_TITLE,
            Self::Project => PROJECT_TITLE,
            Self::Global => GLOBAL_TITLE,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Inventory => " session ",
            Self::Session => " project ",
            Self::Project => " global ",
            Self::Global => " inventory ",
        }
    }
}

pub struct ToolsModal {
    open: bool,
    scope: ToolsScope,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    /// The stats table is as wide as its longest tool name plus nine numeric
    /// columns, which owes nothing to the terminal, so on a small screen this
    /// is what reaches the right-hand columns.
    pan_bar: Scrollbar,
    popup: Rect,
}

impl ToolsModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scope: ToolsScope::default(),
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            popup: Rect::default(),
        }
    }

    pub fn open(&mut self) {
        self.open = true;
        self.scroll.reset();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.scrollbar = Scrollbar::default();
        self.popup = Rect::default();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn scope(&self) -> ToolsScope {
        self.scope
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    /// The scope key is claimed before the scroller sees it: an unhandled key
    /// closes this modal, so leaving it to fall through would dismiss the modal
    /// instead of moving it.
    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
            return;
        }
        if key::SCOPE.matches(key_event) {
            self.scope = self.scope.next();
            self.scroll.reset();
            return;
        }
        if !self.scroll.handle_key(key_event) {
            self.close();
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// A sideways wheel over the modal, which the app routes here rather than
    /// dropping now that the content can run off the edge.
    pub fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return;
            }
        }
        match self.pan_bar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                return;
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::ScrollDown => self.scroll(1),
            _ => {}
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, ctx: &ToolsModalContext) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("tools_modal", area);

        let theme = theme::current();
        // Inventory wraps, because a row is prose that a narrow modal should
        // fold. A table must not: a folded column stops being a column, which
        // is what the pan bar exists to avoid.
        let wrapped = self.scope == ToolsScope::Inventory;
        let lines = match self.scope {
            ToolsScope::Inventory => build_lines(ctx.snapshot, &theme),
            ToolsScope::Session => stats_lines(Some(&ToolStats::from_session(ctx.session)), &theme),
            ToolsScope::Project | ToolsScope::Global => stats_lines(ctx.recorded, &theme),
        };
        let total = match wrapped {
            true => {
                let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
                u16::try_from(paragraph.line_count(content_width(area).max(1)))
                    .unwrap_or(u16::MAX)
                    .min(u16::MAX.saturating_sub(CHROME_LINES))
            }
            false => u16::try_from(lines.len()).unwrap_or(u16::MAX),
        };
        let modal = Modal {
            title: self.scope.title(),
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let horizontal_padding = horizontal_padding(inner.width);
        let padded = Rect {
            x: inner.x.saturating_add(horizontal_padding),
            width: inner
                .width
                .saturating_sub(horizontal_padding.saturating_mul(2)),
            ..inner
        };
        self.scroll.update_dimensions(total, padded.height);
        let content_w = lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX);
        let pan = match wrapped {
            true => 0,
            false => {
                self.scroll.fit_width(content_w, padded.width);
                self.scroll.pan()
            }
        };
        let offset = self.scroll.offset();
        let mut paragraph = Paragraph::new(lines).style(Style::new().fg(theme.foreground));
        if wrapped {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        frame.render_widget(paragraph.scroll((offset, pan)), padded);
        self.scrollbar.draw(frame, inner, total, offset);
        if !wrapped {
            self.pan_bar.draw(frame, bar_area(inner), content_w, pan);
        }
        frame.render_widget(
            Paragraph::new(scope_hint(self.scope, &theme)),
            hint_area(popup),
        );

        self.popup = popup;
        popup
    }
}

/// What the modal reads to answer each scope. `recorded` is `None` when the
/// ledger could not be read, which is different from an empty ledger and says
/// so.
pub struct ToolsModalContext<'a> {
    pub snapshot: Option<&'a ContextSnapshot>,
    pub session: &'a [caudra_storage::sessions::StoredToolUsage],
    pub recorded: Option<&'a ToolStats>,
}

/// Beside the title rather than under the table: the bottom border row belongs
/// to the pan bar, and a hint sharing it would be overpainted exactly when the
/// modal is narrow enough to need both.
fn hint_area(popup: Rect) -> Rect {
    let width = (key::SCOPE.label.len() + ToolsScope::Global.hint().len()) as u16;
    Rect {
        x: popup.x + popup.width.saturating_sub(width + 1),
        y: popup.y,
        width,
        height: 1,
    }
}

fn scope_hint(scope: ToolsScope, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(key::SCOPE.label, theme.keybind_key),
        Span::styled(scope.hint(), theme.tool_dim),
    ])
}

impl Default for ToolsModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for ToolsModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

fn build_lines(snapshot: Option<&ContextSnapshot>, theme: &Theme) -> Vec<Line<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![
            Line::from(Span::styled(NO_SNAPSHOT, theme.status_dim)),
            Line::from(Span::styled(NO_SNAPSHOT_HINT, theme.tool_dim)),
        ];
    };
    let mut lines = summary_lines(snapshot, theme);
    lines.extend(builtin_lines(snapshot, theme));
    lines.extend(mcp_lines(snapshot, theme));
    lines
}

fn summary_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let builtins = &snapshot.inventory.builtins;
    let deferred = builtins.count(ContextBuiltinState::Deferred);
    vec![
        labeled_line(
            "Model",
            escape_terminal_controls(&snapshot.model.spec),
            theme,
        ),
        labeled_line(
            "Declared",
            format!(
                "{} built-in \u{b7} {} in context",
                builtins.count(ContextBuiltinState::Declared),
                token_label(builtins.request_tokens())
            ),
            theme,
        ),
        labeled_line(
            "On demand",
            format!(
                "{deferred} built-in \u{b7} {} if loaded",
                token_label(builtins.deferred_tokens())
            ),
            theme,
        ),
        labeled_line(
            "Off",
            format!("{} built-in", builtins.count(ContextBuiltinState::Disabled)),
            theme,
        ),
    ]
}

fn builtin_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default(), section_line("Built-in", theme)];
    // `tool_search` has no registry entry, so it is derived from the catalog
    // it costs rather than listed like the tools it stands in for.
    let catalog_tokens = snapshot.inventory.builtins.catalog_tokens;
    if catalog_tokens > 0 {
        lines.push(Line::from(vec![
            Span::styled(format!("{DECLARED_GLYPH} "), theme.tool_success),
            Span::styled(TOOL_SEARCH.to_owned(), theme.tool_path),
            Span::styled("  on", theme.tool_success),
            Span::raw(format!(
                " \u{b7} {} in context",
                token_label(catalog_tokens)
            )),
            Span::styled(format!(" \u{b7} {REASON_CATALOG}"), theme.status_dim),
            Span::styled(format!(" \u{b7} {CATALOG_SOURCE}"), theme.status_dim),
        ]));
    }
    for tool in &snapshot.inventory.builtins.tools {
        lines.push(builtin_line(tool, theme));
    }
    lines
}

/// A deferred tool's tokens are a quote, not a charge, so the row says what
/// the number means rather than leaving both readings open.
fn builtin_line(tool: &ContextBuiltinTool, theme: &Theme) -> Line<'static> {
    let (glyph, state, style, tokens) = match tool.state {
        ContextBuiltinState::Declared => (
            DECLARED_GLYPH,
            "on",
            theme.tool_success,
            format!("{} in context", token_label(tool.tokens)),
        ),
        ContextBuiltinState::Deferred => (
            DEFERRED_GLYPH,
            "lazy",
            theme.tool_dim,
            format!("{} if loaded", token_label(tool.tokens)),
        ),
        ContextBuiltinState::Disabled => (DISABLED_GLYPH, "off", theme.error, String::new()),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(escape_terminal_controls(&tool.name), theme.tool_path),
        Span::styled(format!("  {state}"), style),
    ];
    if !tokens.is_empty() {
        spans.push(Span::raw(format!(" \u{b7} {tokens}")));
    }
    if let Some(billed_to) = tool.billed_to {
        spans.push(Span::styled(
            format!(" \u{b7} counted under {billed_to}"),
            theme.status_dim,
        ));
    }
    if let Some(reason) = tool.reason {
        spans.push(Span::styled(format!(" \u{b7} {reason}"), theme.status_dim));
    }
    if !tool.source.is_empty() {
        spans.push(Span::styled(
            format!(" \u{b7} {}", escape_terminal_controls(&tool.source)),
            theme.status_dim,
        ));
    }
    Line::from(spans)
}

fn mcp_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let inventory = &snapshot.inventory.mcp;
    let mut lines = vec![Line::default(), section_line("MCP", theme)];
    if inventory.tools.is_empty() {
        lines.push(Line::from(Span::styled(NO_MCP, theme.status_dim)));
        return lines;
    }
    for tool in &inventory.tools {
        let (glyph, state, style) = match tool.status {
            ContextMcpStatus::LoadedOrEager => (DECLARED_GLYPH, "on", theme.tool_success),
            ContextMcpStatus::AvailableOnDemand => (DEFERRED_GLYPH, "lazy", theme.tool_dim),
            ContextMcpStatus::Disabled => (DISABLED_GLYPH, "off", theme.error),
        };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(
                escape_terminal_controls(&tool.qualified_name),
                theme.tool_path,
            ),
            Span::styled(format!("  {state} \u{b7} "), style),
            Span::raw(token_label(tool.request_tokens)),
            Span::styled(
                format!(" \u{b7} {}", escape_terminal_controls(&tool.server)),
                theme.status_dim,
            ),
        ];
        if let Some(reason) = tool.reason {
            spans.push(Span::styled(
                format!(" · {}", escape_terminal_controls(reason)),
                theme.status_dim,
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The recorded answer for one scope: a totals line, a per-tool table, and a
/// breakdown of why the failing tools failed.
fn stats_lines(stats: Option<&ToolStats>, theme: &Theme) -> Vec<Line<'static>> {
    let Some(stats) = stats else {
        return vec![Line::from(Span::styled(
            STATS_UNAVAILABLE,
            theme.status_dim,
        ))];
    };
    if stats.is_empty() {
        return vec![
            Line::from(Span::styled(NO_ACTIVITY, theme.status_dim)),
            Line::from(Span::styled(NO_ACTIVITY_HINT, theme.tool_dim)),
        ];
    }

    let mut lines = vec![
        labeled_line(
            "Calls",
            format!(
                "{} \u{b7} {} failed \u{b7} {} in results \u{b7} {} of tool time",
                stats.calls,
                stats.errors,
                estimated(stats.tokens),
                duration_label(stats.duration_ms),
            ),
            theme,
        ),
        Line::default(),
    ];

    let tool_w = stats
        .by_tool
        .iter()
        .map(|slice| slice.tool.chars().count())
        .max()
        .unwrap_or(0)
        .max(TOOL_COL_MIN);
    lines.push(header_row(tool_w, theme));
    for slice in &stats.by_tool {
        lines.push(stats_row(slice, stats, tool_w, theme));
    }

    let failing: Vec<&ToolSlice> = stats
        .by_tool
        .iter()
        .filter(|slice| !slice.failures.is_empty())
        .collect();
    if !failing.is_empty() {
        lines.push(Line::default());
        lines.push(section_line(FAILURES_HEADING, theme));
        for slice in failing {
            lines.push(Line::from(vec![
                Span::styled(pad(&slice.tool, tool_w), theme.tool_path),
                Span::styled(failure_summary(slice), theme.error),
            ]));
        }
    }
    lines
}

fn header_row(tool_w: usize, theme: &Theme) -> Line<'static> {
    let mut header = pad("Tool", tool_w);
    for (label, width) in [
        ("Calls", COUNT_COL),
        ("Err", RATE_COL),
        ("Share", RATE_COL),
        ("Tokens", TOKENS_COL),
        ("Tok%", RATE_COL),
        ("Time", TIME_COL),
        ("Time%", RATE_COL),
        ("Avg", TIME_COL),
        ("p50", TIME_COL),
        ("p95", TIME_COL),
    ] {
        header.push_str(&right(label, width));
    }
    Line::from(Span::styled(header, theme.keybind_key))
}

fn stats_row(slice: &ToolSlice, stats: &ToolStats, tool_w: usize, theme: &Theme) -> Line<'static> {
    let mut cells = String::new();
    for (value, width) in [
        (slice.calls.to_string(), COUNT_COL),
        (rate(slice.error_rate()), RATE_COL),
        (rate(share(slice.calls, stats.calls)), RATE_COL),
        (format_tokens_u64(slice.tokens), TOKENS_COL),
        (rate(share(slice.tokens, stats.tokens)), RATE_COL),
        (duration_label(slice.duration_ms), TIME_COL),
        (rate(share(slice.duration_ms, stats.duration_ms)), RATE_COL),
        (
            slice
                .mean_duration_ms()
                .map_or_else(|| NO_SHARE.to_owned(), duration_label),
            TIME_COL,
        ),
        (
            slice
                .p50_duration_ms()
                .map_or_else(|| NO_SHARE.to_owned(), duration_label),
            TIME_COL,
        ),
        (
            slice
                .p95_duration_ms()
                .map_or_else(|| NO_SHARE.to_owned(), duration_label),
            TIME_COL,
        ),
    ] {
        cells.push_str(&right(&value, width));
    }
    Line::from(vec![
        Span::styled(
            pad(&escape_terminal_controls(&slice.tool), tool_w),
            theme.tool_path,
        ),
        Span::raw(cells),
    ])
}

/// Names the classes behind a tool's error rate, so the number reads as a
/// reason rather than as a verdict.
fn failure_summary(slice: &ToolSlice) -> String {
    slice
        .failures
        .iter()
        .map(|(class, count)| format!("{} {count}", outcome_label(*class)))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

fn outcome_label(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Ok => "ok",
        ToolOutcome::Cancelled => "cancelled",
        ToolOutcome::Timeout => "timed out",
        ToolOutcome::Denied => "denied",
        ToolOutcome::NotFound => "not found",
        ToolOutcome::InvalidInput => "bad input",
        ToolOutcome::Other => "failed",
    }
}

fn share(part: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| part as f64 / total as f64)
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(
        || NO_SHARE.to_owned(),
        |value| format!("{:.0}%", value * PERCENT),
    )
}

fn estimated(tokens: u64) -> String {
    format!("{ESTIMATE_MARKER}{}", format_tokens_u64(tokens))
}

fn duration_label(millis: u64) -> String {
    format_settled_duration(Duration::from_millis(millis))
}

fn pad(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    let len = text.chars().count();
    padded.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(len) + COL_GAP,
    ));
    padded
}

fn right(text: &str, width: usize) -> String {
    let len = text.chars().count();
    let mut cell = String::new();
    cell.extend(std::iter::repeat_n(' ', width.saturating_sub(len)));
    cell.push_str(text);
    cell.extend(std::iter::repeat_n(' ', COL_GAP));
    cell
}

fn labeled_line(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), theme.keybind_desc),
        Span::raw(value),
    ])
}

fn section_line(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_owned(), theme.keybind_key))
}

fn content_width(area: Rect) -> u16 {
    let inner = Modal::inner_width(area.width, WIDTH_PERCENT);
    inner.saturating_sub(horizontal_padding(inner).saturating_mul(2))
}

fn horizontal_padding(width: u16) -> u16 {
    if width >= H_PAD_STEP_WIDTH { H_PAD } else { 0 }
}

#[cfg(test)]
mod tests {
    use caudra_agent::context::{
        ContextBuiltinInventory, ContextInventory, ContextMcpInventory, ContextMcpTool,
        ContextModel, ContextReadiness, ContextReserve, ContextUsage, ContextWindow,
    };
    use caudra_agent::tools::profile_policy::PROFILE_LOADING;

    use caudra_storage::sessions::StoredToolUsage;
    use caudra_storage::tool_ledger::Latency;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    use super::{
        CATALOG_SOURCE, COUNT_COL, ContextBuiltinState, ContextBuiltinTool, ContextMcpStatus,
        ContextSnapshot, ESTIMATE_MARKER, FAILURES_HEADING, GLOBAL_TITLE, NO_ACTIVITY,
        NO_ACTIVITY_HINT, NO_MCP, NO_SNAPSHOT, PROJECT_TITLE, REASON_CATALOG, SESSION_TITLE,
        STATS_UNAVAILABLE, TITLE, TOOL_COL_MIN, TOOL_SEARCH, ToolOutcome, ToolStats, ToolsModal,
        ToolsScope, build_lines, key, stats_lines,
    };
    use crate::theme;

    const MODEL_SPEC: &str = "test/model";
    const WINDOW_TOKENS: u32 = 1_000;
    const DECLARED: &str = "file_read";
    const DEFERRED: &str = "code_map";
    const DISABLED: &str = "file_edit";
    const SHELL: &str = "shell";
    const DISABLED_REASON: &str = "model uses the other editing tool";
    const TOOL_SOURCE: &str = "native";
    const P50_COLUMN: &str = "p50";
    const P95_COLUMN: &str = "p95";
    const PERCENTILE_ORDER: &str = "the median column must be named before the tail column";
    const SLICE_TOKENS: u64 = 100;
    const SCOPE_KEEPS_MODAL_OPEN: &str =
        "the scope key must move the modal, not dismiss it like any other unhandled key";

    fn tool(name: &str, state: ContextBuiltinState, tokens: u32) -> ContextBuiltinTool {
        ContextBuiltinTool {
            name: name.to_owned(),
            source: "native:workcell".to_owned(),
            state,
            reason: (state == ContextBuiltinState::Disabled).then_some(DISABLED_REASON),
            tokens,
            billed_to: None,
        }
    }

    fn snapshot(mcp: Vec<ContextMcpTool>) -> ContextSnapshot {
        ContextSnapshot {
            readiness: ContextReadiness::CapturedCurrentRequest,
            model: ContextModel {
                spec: MODEL_SPEC.to_owned(),
                provider_display_name: "Test".to_owned(),
            },
            window: ContextWindow {
                tokens: WINDOW_TOKENS,
                reserve: ContextReserve::Disabled,
            },
            usage: ContextUsage::default(),
            measured: None,
            inventory: ContextInventory {
                builtins: ContextBuiltinInventory {
                    tools: vec![
                        tool(DECLARED, ContextBuiltinState::Declared, 55),
                        tool(DEFERRED, ContextBuiltinState::Deferred, 90),
                        tool(DISABLED, ContextBuiltinState::Disabled, 0),
                    ],
                    catalog_tokens: 20,
                    unattributed_tokens: 0,
                },
                mcp: ContextMcpInventory {
                    tools: mcp,
                    unattributed_tokens: 0,
                },
                ..ContextInventory::default()
            },
        }
    }

    fn join(lines: Vec<ratatui::text::Line<'static>>) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered(snapshot: Option<&ContextSnapshot>) -> String {
        join(build_lines(snapshot, &theme::current()))
    }

    /// A deferred tool costs nothing yet, so its number has to read as a
    /// quote. A disabled one has no number at all and carries the rule
    /// instead.
    #[test]
    fn every_state_says_what_its_number_means() {
        let out = rendered(Some(&snapshot(Vec::new())));
        for expected in [
            "Declared  1 built-in · ~75 tokens in context",
            "On demand  1 built-in · ~90 tokens if loaded",
            "Off  1 built-in",
            "file_read  on · ~55 tokens in context · native:workcell",
            "code_map  lazy · ~90 tokens if loaded · native:workcell",
            "file_edit  off · model uses the other editing tool · native:workcell",
            NO_MCP,
        ] {
            assert!(out.contains(expected), "missing {expected}:\n{out}");
        }
    }

    #[test_case(None; "legacy")]
    #[test_case(Some(PROFILE_LOADING); "profile_reason")]
    fn an_mcp_tool_reports_its_server_and_state(reason: Option<&'static str>) {
        let out = rendered(Some(&snapshot(vec![ContextMcpTool {
            qualified_name: "issues.fetch".to_owned(),
            wire_name: "mcp_issues_fetch".to_owned(),
            server: "issues".to_owned(),
            status: ContextMcpStatus::AvailableOnDemand,
            request_tokens: 0,
            reason,
        }])));
        assert!(out.contains("issues.fetch  lazy · "), "{out}");
        assert!(out.contains("· issues"), "{out}");
        assert!(!out.contains(NO_MCP), "{out}");
        if let Some(reason) = reason {
            assert!(out.contains(reason), "{out}");
        }
    }

    /// `tool_search` has no registry row to inherit, so the report derives it
    /// from the catalog it costs.
    #[test]
    fn the_catalog_is_reported_when_the_request_carries_one() {
        let out = rendered(Some(&snapshot(Vec::new())));

        assert!(
            out.contains(&format!(
                "{TOOL_SEARCH}  on · ~20 tokens in context · {REASON_CATALOG} · {CATALOG_SOURCE}"
            )),
            "{out}"
        );
    }

    #[test]
    fn no_catalog_row_without_a_catalog() {
        let mut without = snapshot(Vec::new());
        without.inventory.builtins.catalog_tokens = 0;

        assert!(
            !rendered(Some(&without)).contains(TOOL_SEARCH),
            "{TOOL_SEARCH}"
        );
    }

    #[test]
    fn no_snapshot_says_so_instead_of_rendering_an_empty_report() {
        assert!(rendered(None).contains(NO_SNAPSHOT));
    }

    fn usage(tool: &str, outcome: ToolOutcome, calls: u64, duration_ms: u64) -> StoredToolUsage {
        StoredToolUsage {
            tool: tool.to_owned(),
            source: TOOL_SOURCE.to_owned(),
            outcome,
            calls,
            duration_ms,
            tokens: calls * SLICE_TOKENS,
            latency: spread(calls, duration_ms),
        }
    }

    /// `duration_ms` is the total, so the histogram gets that many even samples
    /// rather than one call that took the whole of it.
    fn spread(calls: u64, duration_ms: u64) -> Latency {
        let mut latency = Latency::default();
        for _ in 0..calls {
            latency.record(duration_ms / calls.max(1));
        }
        latency
    }

    fn stats_rendered(stats: Option<&ToolStats>) -> String {
        join(stats_lines(stats, &theme::current()))
    }

    #[test]
    fn the_scope_key_cycles_without_dismissing_the_modal() {
        let mut modal = ToolsModal::new();
        modal.open();

        for expected in [
            ToolsScope::Session,
            ToolsScope::Project,
            ToolsScope::Global,
            ToolsScope::Inventory,
        ] {
            modal.handle_key(key::SCOPE.to_key_event());
            assert_eq!(modal.scope(), expected);
            assert!(modal.is_open(), "{SCOPE_KEEPS_MODAL_OPEN}");
        }
    }

    #[test]
    fn an_unhandled_key_still_closes_the_modal() {
        let mut modal = ToolsModal::new();
        modal.open();
        modal.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!modal.is_open());
    }

    #[test]
    fn each_scope_titles_itself() {
        let mut modal = ToolsModal::new();
        let mut titles = vec![modal.scope().title()];
        for _ in 0..3 {
            modal.scope = modal.scope.next();
            titles.push(modal.scope().title());
        }
        assert_eq!(
            titles,
            vec![TITLE, SESSION_TITLE, PROJECT_TITLE, GLOBAL_TITLE]
        );
    }

    #[test]
    fn an_unreadable_ledger_is_not_reported_as_no_activity() {
        let out = stats_rendered(None);
        assert!(out.contains(STATS_UNAVAILABLE), "{out}");
        assert!(!out.contains(NO_ACTIVITY), "{out}");
    }

    #[test]
    fn an_empty_ledger_says_recording_only_just_began() {
        let out = stats_rendered(Some(&ToolStats::default()));
        assert!(out.contains(NO_ACTIVITY), "{out}");
        assert!(out.contains(NO_ACTIVITY_HINT), "{out}");
    }

    /// Every share is taken against the scope's own totals, so the busiest tool
    /// and the slowest one can be told apart at a glance.
    #[test]
    fn a_stats_row_reports_shares_against_the_scope_total() {
        let stats = ToolStats::from_session(&[
            usage(SHELL, ToolOutcome::Ok, 3, 6_000),
            usage(SHELL, ToolOutcome::Timeout, 1, 2_000),
            usage(DECLARED, ToolOutcome::Ok, 6, 600),
        ]);
        let out = stats_rendered(Some(&stats));

        assert!(out.contains("10 · 1 failed"), "{out}");
        assert!(out.contains(ESTIMATE_MARKER), "{out}");
        // Six of ten calls, but 600ms of 8.6s.
        assert!(out.contains("60%"), "{out}");
        assert!(out.contains("7%"), "{out}");
        assert!(out.contains("timed out 1"), "{out}");
    }

    #[test]
    fn a_scope_with_no_failures_prints_no_failure_section() {
        let stats = ToolStats::from_session(&[usage(DECLARED, ToolOutcome::Ok, 2, 10)]);
        let out = stats_rendered(Some(&stats));

        assert!(!out.contains(FAILURES_HEADING), "{out}");
        assert!(out.contains("0%"), "{out}");
    }

    /// The table is wider than a narrow modal, which is what the pan bar is
    /// for; the rows must not be folded to fit.
    #[test]
    fn stats_rows_are_wider_than_they_are_foldable() {
        let stats = ToolStats::from_session(&[usage(DECLARED, ToolOutcome::Ok, 1, 1)]);
        let lines = stats_lines(Some(&stats), &theme::current());
        let header = lines
            .iter()
            .find(|line| line.width() > TOOL_COL_MIN)
            .expect("a table header");

        assert!(header.width() > TOOL_COL_MIN + COUNT_COL);
    }

    /// The median reads as the typical call only when the tail sits beside it.
    #[test]
    fn the_table_names_the_median_before_the_tail() {
        let stats = ToolStats::from_session(&[usage(DECLARED, ToolOutcome::Ok, 4, 400)]);
        let out = stats_rendered(Some(&stats));

        assert!(
            matches!(
                (out.find(P50_COLUMN), out.find(P95_COLUMN)),
                (Some(median), Some(tail)) if median < tail
            ),
            "{PERCENTILE_ORDER}: {out}"
        );
    }
}
