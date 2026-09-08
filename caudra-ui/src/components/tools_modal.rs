use caudra_agent::context::{
    ContextBuiltinState, ContextBuiltinTool, ContextMcpStatus, ContextSnapshot,
};
use caudra_providers::token_label;
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::{ModalScroll, Overlay, escape_terminal_controls};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Tools ";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 82;
const H_PAD: u16 = 2;
const H_PAD_STEP_WIDTH: u16 = 16;
const BORDER_COLUMNS: u16 = 2;
const DECLARED_GLYPH: &str = "\u{25cf}";
const DEFERRED_GLYPH: &str = "\u{25cb}";
const DISABLED_GLYPH: &str = "\u{d7}";
const NO_SNAPSHOT: &str = "No tool report is available yet.";
const NO_SNAPSHOT_HINT: &str = "A report appears when a request is prepared.";
const NO_MCP: &str = "No MCP tools.";

pub struct ToolsModal {
    open: bool,
    scroll: ModalScroll,
    popup: Rect,
}

impl ToolsModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
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
        self.popup = Rect::default();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if !self.scroll.handle_key(key_event) {
            self.close();
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::ScrollDown => self.scroll(1),
            _ => {}
        }
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        snapshot: Option<&ContextSnapshot>,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let lines = build_lines(snapshot, &theme);
        let content_width = content_width(area);
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(content_width.max(1)))
            .unwrap_or(u16::MAX)
            .min(u16::MAX.saturating_sub(CHROME_LINES));
        let modal = Modal {
            title: TITLE,
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
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().fg(theme.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        if total > padded.height {
            render_vertical_scrollbar(frame, inner, total, offset);
        }

        self.popup = popup;
        popup
    }
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
        lines.push(Line::from(vec![
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
        ]));
    }
    lines
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
    let width = area.width.saturating_mul(WIDTH_PERCENT) / 100;
    let inner = width.saturating_sub(BORDER_COLUMNS);
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

    use super::{
        ContextBuiltinState, ContextBuiltinTool, ContextMcpStatus, ContextSnapshot, NO_MCP,
        NO_SNAPSHOT, build_lines,
    };
    use crate::theme;

    const MODEL_SPEC: &str = "test/model";
    const WINDOW_TOKENS: u32 = 1_000;
    const DECLARED: &str = "file_read";
    const DEFERRED: &str = "code_map";
    const DISABLED: &str = "file_edit";
    const DISABLED_REASON: &str = "model uses the other editing tool";

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

    fn rendered(snapshot: Option<&ContextSnapshot>) -> String {
        build_lines(snapshot, &theme::current())
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

    #[test]
    fn an_mcp_tool_reports_its_server_and_state() {
        let out = rendered(Some(&snapshot(vec![ContextMcpTool {
            qualified_name: "issues.fetch".to_owned(),
            wire_name: "mcp_issues_fetch".to_owned(),
            server: "issues".to_owned(),
            status: ContextMcpStatus::AvailableOnDemand,
            request_tokens: 0,
        }])));
        assert!(out.contains("issues.fetch  lazy · "), "{out}");
        assert!(out.contains("· issues"), "{out}");
        assert!(!out.contains(NO_MCP), "{out}");
    }

    #[test]
    fn no_snapshot_says_so_instead_of_rendering_an_empty_report() {
        assert!(rendered(None).contains(NO_SNAPSHOT));
    }
}
