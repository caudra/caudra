use caudra_agent::context::{
    ContextMcpStatus, ContextProfileSource, ContextReadiness, ContextReserve, ContextSnapshot,
};
use caudra_providers::{format_tokens, token_label};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::keybindings::key;
use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::{ModalScroll, Overlay, escape_terminal_controls, hover_style};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Context usage ";
pub(crate) const EXPANDED_TITLE: &str = " Context usage - all ";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 82;
const H_PAD: u16 = 2;
const H_PAD_STEP_WIDTH: u16 = 16;
const BORDER_COLUMNS: u16 = 2;
const GRID_CELL_COUNT: usize = 100;
const CATEGORY_COUNT: usize = 7;
const PERCENT_SCALE: u64 = 100;
const PERCENT_TENTHS_SCALE: u64 = 1_000;
const DIGIT_GROUP: usize = 3;
const LEGEND_GAP: &str = "   ";

pub struct ContextModal {
    open: bool,
    expanded: bool,
    scroll: ModalScroll,
    footer_hit: Rect,
    mouse_position: Option<Position>,
    footer_mouse_down: bool,
}

impl ContextModal {
    pub fn new() -> Self {
        Self {
            open: false,
            expanded: false,
            scroll: ModalScroll::new_top(),
            footer_hit: Rect::default(),
            mouse_position: None,
            footer_mouse_down: false,
        }
    }

    pub fn open(&mut self, expanded: bool) {
        self.open = true;
        self.expanded = expanded;
        self.scroll.reset();
        self.footer_hit = Rect::default();
        self.footer_mouse_down = false;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.footer_hit = Rect::default();
        self.mouse_position = None;
        self.footer_mouse_down = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if matches!(key_event.code, KeyCode::Esc | KeyCode::Char('q'))
            || key::QUIT.matches(key_event)
        {
            self.close();
        } else {
            self.scroll.handle_key(key_event);
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        let position = Position::new(event.column, event.row);
        self.mouse_position = Some(position);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.footer_mouse_down = self.footer_hit.contains(position);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.footer_mouse_down = false;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let activate = self.footer_mouse_down && self.footer_hit.contains(position);
                self.footer_mouse_down = false;
                if activate {
                    self.open(!self.expanded);
                }
            }
            _ => {}
        }
    }

    fn footer_hovered(&self) -> bool {
        self.mouse_position
            .is_some_and(|position| self.footer_hit.contains(position))
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self) -> Rect {
        self.footer_hit
    }

    fn update_footer_hit(&mut self, lines: &[Line<'static>], area: Rect, offset: u16, total: u16) {
        self.footer_hit = Rect::default();
        let Some(footer) = lines.last() else {
            return;
        };
        let Ok(footer_width) = u16::try_from(footer.width()) else {
            return;
        };
        if area.width == 0 || footer_width > area.width {
            return;
        }
        let Some(row) = total.saturating_sub(1).checked_sub(offset) else {
            return;
        };
        if row >= area.height {
            return;
        }
        let command_width = u16::try_from(footer_command(self.expanded).len()).unwrap_or(u16::MAX);
        self.footer_hit = Rect::new(
            area.x
                .saturating_add(area.width.saturating_sub(footer_width) / 2),
            area.y.saturating_add(row),
            command_width.min(area.width),
            1,
        );
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

        let content_width = content_width(area);
        let theme = theme::current();
        let mut lines = build_lines(snapshot, self.expanded, content_width, &theme);
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let visual_height = paragraph.line_count(content_width.max(1));
        let total = u16::try_from(visual_height)
            .unwrap_or(u16::MAX)
            .min(u16::MAX.saturating_sub(CHROME_LINES));
        let modal = Modal {
            title: if self.expanded { EXPANDED_TITLE } else { TITLE },
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
        self.update_footer_hit(&lines, padded, offset, total);
        if self.footer_hovered() {
            lines =
                build_lines_with_footer_hover(snapshot, self.expanded, content_width, &theme, true);
        }

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

        popup
    }
}

impl Default for ContextModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for ContextModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridKind {
    SystemPrompt,
    SystemTools,
    McpTools,
    Profiles,
    Memory,
    Skills,
    Messages,
    Free,
    Reserve,
}

impl GridKind {
    fn glyph(self) -> &'static str {
        match self {
            Self::SystemPrompt => "S",
            Self::SystemTools => "T",
            Self::McpTools => "M",
            Self::Profiles => "P",
            Self::Memory => "R",
            Self::Skills => "K",
            Self::Messages => "C",
            Self::Free => "·",
            Self::Reserve => "░",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SystemPrompt => "System prompt",
            Self::SystemTools => "System tools",
            Self::McpTools => "MCP tools",
            Self::Profiles => "Profiles",
            Self::Memory => "Memory",
            Self::Skills => "Skills",
            Self::Messages => "Messages",
            Self::Free => "Free",
            Self::Reserve => "Reserve",
        }
    }

    fn style(self, theme: &Theme) -> Style {
        match self {
            Self::SystemPrompt => theme.heading,
            Self::SystemTools => theme.tool,
            Self::McpTools => theme.tool_path,
            Self::Profiles => theme.accent,
            Self::Memory => theme.status_notice,
            Self::Skills => theme.todo_in_progress,
            Self::Messages => theme.assistant,
            Self::Free => theme.tool_dim,
            Self::Reserve => theme.todo_pending,
        }
    }
}

fn build_lines(
    snapshot: Option<&ContextSnapshot>,
    expanded: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    build_lines_with_footer_hover(snapshot, expanded, width, theme, false)
}

fn build_lines_with_footer_hover(
    snapshot: Option<&ContextSnapshot>,
    expanded: bool,
    width: u16,
    theme: &Theme,
    footer_hovered: bool,
) -> Vec<Line<'static>> {
    let mut lines = if let Some(snapshot) = snapshot {
        let mut lines = summary_lines(snapshot, width, theme);
        if expanded {
            lines.extend(mcp_lines(snapshot, theme));
            lines.extend(profile_lines(snapshot, theme));
            lines.extend(memory_lines(snapshot, theme));
            lines.extend(skill_lines(snapshot, theme));
        }
        lines
    } else {
        vec![
            Line::from(Span::styled(
                "No context snapshot is available yet.",
                theme.status_dim,
            )),
            Line::from(Span::styled(
                "A snapshot appears when a request is prepared.",
                theme.tool_dim,
            )),
        ]
    };
    lines.push(Line::default());
    lines.push(command_hint(expanded, theme, footer_hovered));
    lines
}

fn summary_lines(snapshot: &ContextSnapshot, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let used = snapshot.usage.used();
    let window = snapshot.window.tokens;
    let mut lines = vec![
        labeled_line(
            "Model",
            escape_terminal_controls(&snapshot.model.spec),
            theme,
        ),
        labeled_line(
            "Provider",
            escape_terminal_controls(&snapshot.model.provider_display_name),
            theme,
        ),
        labeled_line("Window", format!("{} tokens", format_tokens(window)), theme),
        labeled_line(
            "Used",
            format!(
                "{} / {} tokens ({}%)",
                token_label(used),
                format_tokens(window),
                snapshot.usage.percentage(&snapshot.window)
            ),
            theme,
        ),
    ];

    if snapshot.readiness == ContextReadiness::PreparedNextRequest {
        lines.push(Line::from(vec![
            Span::styled("◇ ", theme.status_notice),
            Span::styled(
                "Prepared for the next request; counts may change before send.",
                theme.status_dim,
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(section_line("Allocation · 100 cells · 1 cell = 1%", theme));
    lines.extend(grid_lines(snapshot, width, theme));
    lines.extend(legend_lines(snapshot, width, theme));
    lines.push(compaction_line(snapshot, theme));

    let over_window = snapshot.usage.over_window(&snapshot.window);
    if over_window > 0 {
        lines.push(capacity_warning(
            format!("{} over the model window", token_label(over_window)),
            theme.error,
        ));
    } else if snapshot.window.reserve.is_enabled() {
        let over_threshold = used.saturating_sub(snapshot.usage.threshold(&snapshot.window));
        if over_threshold > 0 {
            lines.push(capacity_warning(
                format!(
                    "{} over the auto-compact threshold",
                    token_label(over_threshold)
                ),
                theme.todo_in_progress,
            ));
        }
    }

    lines.push(Line::default());
    lines.push(section_line("Inventory", theme));
    lines.extend(inventory_summary_lines(snapshot, theme));
    lines
}

fn labeled_line(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), theme.tool_dim),
        Span::raw(value),
    ])
}

fn section_line(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_owned(), theme.keybind_section))
}

fn capacity_warning(message: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("! ", style),
        Span::styled(message, style),
    ])
}

fn compaction_line(snapshot: &ContextSnapshot, theme: &Theme) -> Line<'static> {
    match snapshot.window.reserve {
        ContextReserve::Disabled => labeled_line("Auto-compact", "disabled".to_owned(), theme),
        ContextReserve::Enabled(reserve) => {
            let threshold = snapshot.usage.threshold(&snapshot.window);
            labeled_line(
                "Auto-compact",
                format!(
                    "threshold {} tokens ({}) · reserve {} tokens ({})",
                    format_integer(u64::from(threshold)),
                    format_percentage(threshold, snapshot.window.tokens),
                    format_integer(u64::from(reserve)),
                    format_percentage(reserve, snapshot.window.tokens)
                ),
                theme,
            )
        }
    }
}

fn footer_command(expanded: bool) -> &'static str {
    if expanded { "/context" } else { "/context all" }
}

fn command_hint(expanded: bool, theme: &Theme, hovered: bool) -> Line<'static> {
    let description = if expanded {
        " summary"
    } else {
        " item details"
    };
    Line::from(vec![
        Span::styled(
            footer_command(expanded),
            hover_style(theme.keybind_key, hovered),
        ),
        Span::styled(description, theme.tool_dim),
        Span::styled(" · Esc close", theme.tool_dim),
    ])
    .alignment(Alignment::Center)
}

fn usage_categories(snapshot: &ContextSnapshot) -> [(GridKind, u32); CATEGORY_COUNT] {
    [
        (GridKind::SystemPrompt, snapshot.usage.system_prompt),
        (GridKind::SystemTools, snapshot.usage.system_tools),
        (GridKind::McpTools, snapshot.usage.mcp_tools),
        (GridKind::Profiles, snapshot.usage.profiles),
        (GridKind::Memory, snapshot.usage.memory),
        (GridKind::Skills, snapshot.usage.skills),
        (GridKind::Messages, snapshot.usage.messages),
    ]
}

fn grid_cells(snapshot: &ContextSnapshot) -> Vec<GridKind> {
    let window = snapshot.window.tokens;
    if window == 0 {
        return vec![GridKind::Free; GRID_CELL_COUNT];
    }

    let threshold = snapshot.usage.threshold(&snapshot.window);
    let displayed_used = snapshot.usage.used().min(window);
    let free = threshold.saturating_sub(displayed_used);
    let reserve = window.saturating_sub(displayed_used).saturating_sub(free);
    let capacity = apportion(
        &[
            u64::from(displayed_used),
            u64::from(free),
            u64::from(reserve),
        ],
        GRID_CELL_COUNT,
    );
    let categories = usage_categories(snapshot);
    let category_weights = categories
        .iter()
        .map(|(_, tokens)| u64::from(*tokens))
        .collect::<Vec<_>>();
    let category_cells = apportion(&category_weights, capacity[0]);
    let mut cells = Vec::with_capacity(GRID_CELL_COUNT);
    for ((kind, _), count) in categories.into_iter().zip(category_cells) {
        cells.extend(std::iter::repeat_n(kind, count));
    }
    cells.extend(std::iter::repeat_n(GridKind::Free, capacity[1]));
    cells.extend(std::iter::repeat_n(GridKind::Reserve, capacity[2]));
    cells.resize(GRID_CELL_COUNT, GridKind::Free);
    cells.truncate(GRID_CELL_COUNT);
    cells
}

fn apportion(weights: &[u64], cells: usize) -> Vec<usize> {
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

fn grid_lines(snapshot: &ContextSnapshot, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let row_width = usize::from(width).min(GRID_CELL_COUNT);
    grid_cells(snapshot)
        .chunks(row_width)
        .map(|row| {
            let mut spans = Vec::new();
            let mut start = 0;
            while start < row.len() {
                let kind = row[start];
                let mut end = start + 1;
                while end < row.len() && row[end] == kind {
                    end += 1;
                }
                spans.push(Span::styled(
                    kind.glyph().repeat(end - start),
                    kind.style(theme),
                ));
                start = end;
            }
            Line::from(spans)
        })
        .collect()
}

fn legend_lines(snapshot: &ContextSnapshot, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut entries = usage_categories(snapshot)
        .into_iter()
        .map(|(kind, tokens)| legend_entry(kind, tokens, snapshot.window.tokens, theme))
        .collect::<Vec<_>>();
    entries.push(legend_entry(
        GridKind::Free,
        snapshot.usage.free(&snapshot.window),
        snapshot.window.tokens,
        theme,
    ));
    let mut reserve = legend_entry(
        GridKind::Reserve,
        snapshot.window.reserve.tokens(),
        snapshot.window.tokens,
        theme,
    );
    if snapshot.window.reserve == ContextReserve::Disabled {
        reserve.push(Span::styled(" disabled", theme.tool_dim));
    }
    entries.push(reserve);

    let max_width = usize::from(width.max(1));
    let gap_width = LEGEND_GAP.len();
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0_usize;
    for entry in entries {
        let entry_width = Line::from(entry.clone()).width();
        let next_width = current_width
            .saturating_add(if current.is_empty() { 0 } else { gap_width })
            .saturating_add(entry_width);
        if !current.is_empty() && next_width > max_width {
            lines.push(Line::from(current));
            current = Vec::new();
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(Span::raw(LEGEND_GAP));
            current_width = current_width.saturating_add(gap_width);
        }
        current_width = current_width.saturating_add(entry_width);
        current.extend(entry);
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

fn legend_entry(kind: GridKind, tokens: u32, window: u32, theme: &Theme) -> Vec<Span<'static>> {
    let token_value = if kind == GridKind::Reserve {
        format!("{} tokens", format_integer(u64::from(tokens)))
    } else {
        token_label(tokens)
    };
    vec![
        Span::styled(kind.glyph(), kind.style(theme)),
        Span::styled(format!(" {} ", kind.label()), theme.keybind_desc),
        Span::raw(format!(
            "{token_value} ({})",
            format_percentage(tokens, window)
        )),
    ]
}

fn inventory_summary_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let mcp = &snapshot.inventory.mcp;
    let (loaded_mcp, deferred_mcp, disabled_mcp) = mcp_status_counts(snapshot);
    let profiles = &snapshot.inventory.profiles;
    let current_profiles = profiles
        .profiles
        .iter()
        .filter(|profile| profile.active)
        .map(|profile| escape_terminal_controls(&profile.name))
        .collect::<Vec<_>>();
    let current_profiles = if current_profiles.is_empty() {
        "none".to_owned()
    } else {
        current_profiles.join(", ")
    };
    let task_profiles = profiles
        .profiles
        .iter()
        .filter(|profile| profile.available_for_tasks)
        .count();
    let memory = &snapshot.inventory.memory;
    let skills = &snapshot.inventory.skills;
    let loaded_skills = skills
        .skills
        .iter()
        .filter(|skill| skill.loaded_tokens > 0)
        .count();

    vec![
        inventory_summary_line(
            GridKind::McpTools,
            format!(
                "{} tools · {} loaded/eager · {} on demand · {} disabled · {} in context",
                format_usize(mcp.tools.len()),
                format_usize(loaded_mcp),
                format_usize(deferred_mcp),
                format_usize(disabled_mcp),
                token_label(mcp.request_tokens())
            ),
            theme,
        ),
        inventory_summary_line(
            GridKind::Profiles,
            format!(
                "{} profiles · current {} · {} task-available · {} in context",
                format_usize(profiles.profiles.len()),
                current_profiles,
                format_usize(task_profiles),
                token_label(profiles.request_tokens)
            ),
            theme,
        ),
        inventory_summary_line(
            GridKind::Memory,
            format!(
                "{} notes · {} on load · {} in context · {} unreadable",
                format_usize(memory.files.len()),
                token_label(memory.on_load_tokens()),
                token_label(memory.request_tokens()),
                format_usize(memory.unreadable_files)
            ),
            theme,
        ),
        inventory_summary_line(
            GridKind::Skills,
            format!(
                "{} skills · {} loaded · definitions {} · loaded bodies {}",
                format_usize(skills.skills.len()),
                format_usize(loaded_skills),
                token_label(skills.definition_tokens),
                token_label(skills.loaded_tokens)
            ),
            theme,
        ),
    ]
}

fn inventory_summary_line(kind: GridKind, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{} ", kind.glyph()), kind.style(theme)),
        Span::styled(format!("{}  ", kind.label()), theme.keybind_desc),
        Span::raw(value),
    ])
}

fn mcp_status_counts(snapshot: &ContextSnapshot) -> (usize, usize, usize) {
    snapshot
        .inventory
        .mcp
        .tools
        .iter()
        .fold((0, 0, 0), |(loaded, deferred, disabled), tool| {
            match tool.status {
                ContextMcpStatus::LoadedOrEager => (loaded.saturating_add(1), deferred, disabled),
                ContextMcpStatus::AvailableOnDemand => {
                    (loaded, deferred.saturating_add(1), disabled)
                }
                ContextMcpStatus::Disabled => (loaded, deferred, disabled.saturating_add(1)),
            }
        })
}

fn mcp_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let inventory = &snapshot.inventory.mcp;
    let mut lines = vec![
        Line::default(),
        section_line("MCP tools", theme),
        labeled_line("In context", token_label(inventory.request_tokens()), theme),
        labeled_line(
            "Search catalog",
            token_label(inventory.catalog_tokens),
            theme,
        ),
        labeled_line(
            "Unattributed",
            token_label(inventory.unattributed_tokens),
            theme,
        ),
    ];
    if inventory.tools.is_empty() {
        lines.push(Line::from(Span::styled("No MCP tools.", theme.status_dim)));
        return lines;
    }

    let mut tools = inventory.tools.iter().collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        left.status
            .cmp(&right.status)
            .then_with(|| left.server.cmp(&right.server))
            .then_with(|| left.qualified_name.cmp(&right.qualified_name))
    });
    for tool in tools {
        let (glyph, status, status_style) = match tool.status {
            ContextMcpStatus::LoadedOrEager => ("●", "loaded/eager", theme.tool_success),
            ContextMcpStatus::AvailableOnDemand => ("○", "on demand", theme.tool_dim),
            ContextMcpStatus::Disabled => ("×", "disabled", theme.error),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), status_style),
            Span::styled(
                escape_terminal_controls(&tool.qualified_name),
                theme.tool_path,
            ),
            Span::styled(format!("  {status} · "), status_style),
            Span::raw(token_label(tool.request_tokens)),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "  server {} · wire {}",
                escape_terminal_controls(&tool.server),
                escape_terminal_controls(&tool.wire_name)
            ),
            theme.status_dim,
        )));
    }
    lines
}

fn profile_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let inventory = &snapshot.inventory.profiles;
    let mut lines = vec![
        Line::default(),
        section_line("Profiles", theme),
        labeled_line("In context", token_label(inventory.request_tokens), theme),
    ];
    if inventory.profiles.is_empty() {
        lines.push(Line::from(Span::styled(
            "No profiles found.",
            theme.status_dim,
        )));
        return lines;
    }

    let mut profiles = inventory.profiles.iter().collect::<Vec<_>>();
    profiles.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then_with(|| left.name.cmp(&right.name))
    });
    for profile in profiles {
        let (glyph, marker_style) = if profile.active {
            ("●", theme.status_notice)
        } else if !profile.available_for_tasks {
            ("×", theme.error)
        } else {
            ("○", theme.tool_dim)
        };
        let source = match profile.source {
            ContextProfileSource::Builtin => "built-in",
            ContextProfileSource::User => "user",
        };
        let availability = if profile.available_for_tasks {
            "tasks available".to_owned()
        } else if let Some(reason) = &profile.task_unavailable_reason {
            format!("tasks unavailable: {}", escape_terminal_controls(reason))
        } else {
            "tasks unavailable".to_owned()
        };
        let current = if profile.active { " · current" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), marker_style),
            Span::styled(escape_terminal_controls(&profile.name), theme.accent),
            Span::styled(
                format!("{current} · {source} · {availability}"),
                theme.status_dim,
            ),
        ]));
        if let Some(description) = &profile.description {
            lines.push(Line::from(Span::styled(
                format!("  {}", escape_terminal_controls(description)),
                theme.item_desc,
            )));
        }
    }
    lines
}

fn memory_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let inventory = &snapshot.inventory.memory;
    let directory = inventory.directory.as_ref().map_or_else(
        || "unavailable".to_owned(),
        |path| escape_terminal_controls(&path.display().to_string()),
    );
    let mut lines = vec![
        Line::default(),
        section_line("Memory", theme),
        labeled_line("Directory", directory, theme),
        labeled_line(
            "On-load bodies",
            format!(
                "{} across {} notes",
                token_label(inventory.on_load_tokens()),
                format_usize(inventory.files.len())
            ),
            theme,
        ),
        labeled_line(
            "In context",
            format!(
                "{} · definition {} · tag index {} · loaded bodies {}",
                token_label(inventory.request_tokens()),
                token_label(inventory.definition_tokens),
                token_label(inventory.prompt_tokens),
                token_label(inventory.loaded_tokens)
            ),
            theme,
        ),
    ];
    if inventory.unreadable_files > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "! {} unreadable files",
                format_usize(inventory.unreadable_files)
            ),
            theme.error,
        )));
    }
    if inventory.files.is_empty() {
        lines.push(Line::from(Span::styled(
            "No memory notes found.",
            theme.status_dim,
        )));
        return lines;
    }

    let mut files = inventory.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    for file in files {
        lines.push(Line::from(vec![
            Span::styled("○ ", theme.tool_dim),
            Span::styled(escape_terminal_controls(&file.name), theme.tool_path),
            Span::styled("  on load · ", theme.status_dim),
            Span::raw(token_label(file.on_load_tokens)),
        ]));
        let tags = if file.tags.is_empty() {
            "no tags".to_owned()
        } else {
            file.tags
                .iter()
                .map(|tag| escape_terminal_controls(tag))
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(Line::from(Span::styled(
            format!("  tags: {tags}"),
            theme.item_desc,
        )));
    }
    lines
}

fn skill_lines(snapshot: &ContextSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let inventory = &snapshot.inventory.skills;
    let mut lines = vec![
        Line::default(),
        section_line("Skills", theme),
        labeled_line(
            "Definition index",
            format!("{} in context", token_label(inventory.definition_tokens)),
            theme,
        ),
        labeled_line(
            "Loaded bodies",
            format!("{} in context", token_label(inventory.loaded_tokens)),
            theme,
        ),
    ];
    if inventory.skills.is_empty() {
        lines.push(Line::from(Span::styled(
            "No skills found.",
            theme.status_dim,
        )));
        return lines;
    }

    let mut skills = inventory.skills.iter().collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    for skill in skills {
        let loaded = skill.loaded_tokens > 0;
        let (glyph, marker_style, state) = if loaded {
            ("●", theme.tool_success, "loaded")
        } else {
            ("○", theme.tool_dim, "available on demand")
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), marker_style),
            Span::styled(escape_terminal_controls(&skill.name), theme.accent),
            Span::styled(format!("  {state} · "), marker_style),
            Span::raw(token_label(skill.loaded_tokens)),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  {}", escape_terminal_controls(&skill.description)),
            theme.item_desc,
        )));
    }
    lines
}

fn content_width(area: Rect) -> u16 {
    let popup_width = u32::from(area.width) * u32::from(WIDTH_PERCENT) / PERCENT_SCALE as u32;
    let inner_width = u16::try_from(popup_width)
        .unwrap_or(u16::MAX)
        .saturating_sub(BORDER_COLUMNS);
    let horizontal_padding = horizontal_padding(inner_width);
    inner_width.saturating_sub(horizontal_padding.saturating_mul(2))
}

fn horizontal_padding(width: u16) -> u16 {
    (width / H_PAD_STEP_WIDTH).min(H_PAD)
}

fn format_percentage(tokens: u32, window: u32) -> String {
    if window == 0 {
        return "0.0%".to_owned();
    }
    let tenths = u64::from(tokens).saturating_mul(PERCENT_TENTHS_SCALE) / u64::from(window);
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}

fn format_usize(value: usize) -> String {
    format_integer(u64::try_from(value).unwrap_or(u64::MAX))
}

fn format_integer(value: u64) -> String {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_agent::context::{
        ContextInventory, ContextMcpInventory, ContextMcpTool, ContextMemoryFile,
        ContextMemoryInventory, ContextModel, ContextProfile, ContextProfileInventory,
        ContextSkill, ContextSkillInventory, ContextUsage, ContextWindow,
    };
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::*;
    use crate::components::key as key_event;

    const MODEL_SPEC: &str = "test/large";
    const MCP_TOOL: &str = "issues.fetch";
    const PROFILE: &str = "default";
    const MEMORY_FILE: &str = "project.md";
    const SKILL: &str = "deploy";

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            readiness: ContextReadiness::PreparedNextRequest,
            model: ContextModel {
                spec: MODEL_SPEC.to_owned(),
                provider_display_name: "Test Provider".to_owned(),
            },
            window: ContextWindow {
                tokens: 1_000,
                reserve: ContextReserve::Enabled(100),
            },
            usage: ContextUsage {
                system_prompt: 100,
                system_tools: 80,
                mcp_tools: 60,
                profiles: 40,
                memory: 50,
                skills: 70,
                messages: 200,
            },
            inventory: ContextInventory {
                profiles: ContextProfileInventory {
                    profiles: vec![ContextProfile {
                        name: PROFILE.to_owned(),
                        description: Some("The built-in profile".to_owned()),
                        source: ContextProfileSource::Builtin,
                        active: true,
                        available_for_tasks: true,
                        task_unavailable_reason: None,
                    }],
                    request_tokens: 40,
                },
                memory: ContextMemoryInventory {
                    directory: Some(PathBuf::from("/tmp/memory")),
                    files: vec![ContextMemoryFile {
                        name: MEMORY_FILE.to_owned(),
                        tags: vec!["project".to_owned()],
                        on_load_tokens: 120,
                    }],
                    unreadable_files: 1,
                    definition_tokens: 10,
                    prompt_tokens: 15,
                    loaded_tokens: 25,
                },
                skills: ContextSkillInventory {
                    skills: vec![ContextSkill {
                        name: SKILL.to_owned(),
                        description: "Deploy safely".to_owned(),
                        loaded_tokens: 45,
                    }],
                    definition_tokens: 25,
                    loaded_tokens: 45,
                },
                mcp: ContextMcpInventory {
                    tools: vec![
                        ContextMcpTool {
                            qualified_name: MCP_TOOL.to_owned(),
                            wire_name: "mcp_issues_fetch".to_owned(),
                            server: "issues".to_owned(),
                            status: ContextMcpStatus::LoadedOrEager,
                            request_tokens: 40,
                        },
                        ContextMcpTool {
                            qualified_name: "search.query".to_owned(),
                            wire_name: "mcp_search_query".to_owned(),
                            server: "search".to_owned(),
                            status: ContextMcpStatus::AvailableOnDemand,
                            request_tokens: 0,
                        },
                        ContextMcpTool {
                            qualified_name: "admin.delete".to_owned(),
                            wire_name: "mcp_admin_delete".to_owned(),
                            server: "admin".to_owned(),
                            status: ContextMcpStatus::Disabled,
                            request_tokens: 0,
                        },
                    ],
                    catalog_tokens: 20,
                    unattributed_tokens: 0,
                },
            },
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
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

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn summary_and_all_views_have_the_expected_scope_and_stable_sections() {
        let snapshot = snapshot();
        let theme = theme::current();
        let summary = text(&build_lines(Some(&snapshot), false, 100, &theme));
        assert!(summary.contains(MODEL_SPEC));
        assert!(summary.contains("Prepared for the next request"));
        assert!(summary.contains("/context all"));
        assert!(!summary.contains(MCP_TOOL));

        let all = build_lines(Some(&snapshot), true, 100, &theme);
        let all_text = text(&all);
        for item in [MCP_TOOL, PROFILE, MEMORY_FILE, SKILL] {
            assert!(all_text.contains(item), "missing {item}: {all_text}");
        }
        let positions = ["MCP tools", "Profiles", "Memory", "Skills"].map(|section| {
            all.iter()
                .position(|line| text(std::slice::from_ref(line)) == section)
                .unwrap_or_else(|| panic!("missing section {section}"))
        });
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn footer_command_hovers_and_switches_views() {
        const WIDTH: u16 = 120;
        const HEIGHT: u16 = 50;

        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut modal = ContextModal::new();
        let snapshot = snapshot();
        modal.open(false);
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), Some(&snapshot));
            })
            .unwrap();

        let summary_hit = modal.footer_hit();
        assert!(!summary_hit.is_empty());
        modal.handle_mouse(mouse(MouseEventKind::Moved, summary_hit));
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), Some(&snapshot));
            })
            .unwrap();
        let reversed = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| Position::new(x, y)))
            .filter(|position| {
                terminal.backend().buffer()[(position.x, position.y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect::<Vec<_>>();
        assert!(
            summary_hit.contains(reversed[0]),
            "hit={summary_hit:?} reversed={reversed:?}"
        );

        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), summary_hit));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), summary_hit));
        assert!(modal.expanded);

        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), Some(&snapshot));
            })
            .unwrap();
        modal.scroll(-i32::MAX);
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), Some(&snapshot));
            })
            .unwrap();
        let expanded_hit = modal.footer_hit();
        assert!(!expanded_hit.is_empty());
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), expanded_hit));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), expanded_hit));
        assert!(!modal.expanded);
    }

    #[test_case(1, 100 ; "one_column")]
    #[test_case(7, 15 ; "narrow_wrapping")]
    #[test_case(100, 1 ; "full_grid_row")]
    fn grid_wraps_without_losing_cells(width: u16, expected_rows: usize) {
        let snapshot = snapshot();
        let lines = grid_lines(&snapshot, width, &theme::current());
        assert_eq!(lines.len(), expected_rows);
        assert_eq!(
            lines.iter().map(Line::width).sum::<usize>(),
            GRID_CELL_COUNT
        );
        assert!(
            lines
                .iter()
                .all(|line| line.width() <= usize::from(width.max(1)))
        );
    }

    #[test]
    fn zero_width_omits_the_invisible_grid() {
        assert!(grid_lines(&snapshot(), 0, &theme::current()).is_empty());
    }

    #[test_case(15, 0 ; "narrow_content_keeps_every_column")]
    #[test_case(16, 1 ; "medium_content_uses_compact_padding")]
    #[test_case(31, 1 ; "medium_content_stays_compact")]
    #[test_case(32, H_PAD ; "wide_content_uses_full_padding")]
    fn horizontal_padding_responds_to_available_width(width: u16, expected: u16) {
        assert_eq!(horizontal_padding(width), expected);
    }

    #[test]
    fn token_estimates_are_marked_without_marking_capacities_or_counts() {
        let rendered = text(&build_lines(
            Some(&snapshot()),
            true,
            GRID_CELL_COUNT as u16,
            &theme::current(),
        ));
        for expected in [
            "Window  1.0k tokens",
            "Used  ~600 tokens / 1.0k tokens (60%)",
            "S System prompt ~100 tokens (10.0%)",
            "· Free ~300 tokens (30.0%)",
            "░ Reserve 100 tokens (10.0%)",
            "Auto-compact  threshold 900 tokens (90.0%) · reserve 100 tokens (10.0%)",
            "M MCP tools  3 tools · 1 loaded/eager · 1 on demand · 1 disabled · ~60 tokens in context",
            "On-load bodies  ~120 tokens across 1 notes",
            "definitions ~25 tokens · loaded bodies ~45 tokens",
            "issues.fetch  loaded/eager · ~40 tokens",
            "project.md  on load · ~120 tokens",
            "deploy  loaded · ~45 tokens",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        for unexpected in [
            "Window  ~1.0k tokens",
            "reserve ~100 tokens",
            "~3 tools",
            "~1 notes",
        ] {
            assert!(
                !rendered.contains(unexpected),
                "unexpected {unexpected}: {rendered}"
            );
        }
    }

    #[test]
    fn zero_window_math_is_defined() {
        let mut snapshot = snapshot();
        snapshot.window.tokens = 0;
        snapshot.usage.messages = u32::MAX;
        let lines = build_lines(Some(&snapshot), false, 0, &theme::current());
        assert!(text(&lines).contains("(0%)"));
        assert_eq!(grid_cells(&snapshot).len(), GRID_CELL_COUNT);
    }

    #[test]
    fn narrow_and_zero_areas_render_safely() {
        let snapshot = snapshot();
        let backend = TestBackend::new(8, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut modal = ContextModal::new();
        modal.open(false);

        terminal
            .draw(|frame| {
                modal.view(
                    frame,
                    Rect {
                        width: 1,
                        height: 1,
                        ..Rect::default()
                    },
                    Some(&snapshot),
                );
            })
            .unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, Rect::default(), Some(&snapshot));
            })
            .unwrap();
    }

    #[test]
    fn over_window_usage_is_reported_and_clamped_to_the_grid() {
        let mut snapshot = snapshot();
        snapshot.readiness = ContextReadiness::CapturedCurrentRequest;
        snapshot.window.tokens = 100;
        snapshot.window.reserve = ContextReserve::Disabled;
        snapshot.usage = ContextUsage {
            messages: 150,
            ..ContextUsage::default()
        };
        let lines = build_lines(Some(&snapshot), false, 100, &theme::current());
        let rendered = text(&lines);
        assert!(rendered.contains("(150%)"), "{rendered}");
        assert!(rendered.contains("~50 tokens over the model window"));
        assert!(
            grid_cells(&snapshot)
                .iter()
                .all(|kind| *kind == GridKind::Messages)
        );
    }

    #[test]
    fn disabled_reserve_has_no_grid_cells_and_is_named() {
        let mut snapshot = snapshot();
        snapshot.window.reserve = ContextReserve::Disabled;
        let lines = build_lines(Some(&snapshot), false, 100, &theme::current());
        assert!(text(&lines).contains("Auto-compact  disabled"));
        assert!(!grid_cells(&snapshot).contains(&GridKind::Reserve));
    }

    #[test]
    fn scrolling_and_close_reset_modal_state() {
        let snapshot = snapshot();
        let backend = TestBackend::new(80, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut modal = ContextModal::new();
        modal.open(true);
        assert!(modal.is_open());
        assert!(modal.expanded);
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), Some(&snapshot));
            })
            .unwrap();

        modal.scroll(-5);
        assert!(modal.scroll.offset() > 0);
        modal.handle_key(key_event(KeyCode::Esc));
        assert!(!modal.is_open());
        assert_eq!(modal.scroll.offset(), 0);

        modal.open(false);
        assert!(!modal.expanded);
        modal.handle_key(key_event(KeyCode::Char('q')));
        assert!(!modal.is_open());
    }
}
