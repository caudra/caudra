use std::mem;
use std::sync::Arc;

use caudra_agent::command::CustomCommand;
use caudra_agent::{McpPromptInfo, McpSnapshotReader};
use caudra_lua::{LuaCommandInfo, LuaCommandReader};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::components::list_picker::PickerItem;
use crate::theme;

const TICK_TIMEOUT_MS: u64 = 10;
pub(crate) const SECTION_BUILTIN: &str = "Built-in";
const SECTION_CUSTOM: &str = "Project & User";
const SECTION_MCP: &str = "MCP Prompts";
const SECTION_PLUGIN: &str = "Plugins";

/// A command as the modal palette sees it: flat, owned, and independent of
/// the index-based [`CommandType`] the inline dropdown matches against.
#[derive(Clone)]
pub struct CommandRow {
    pub name: String,
    pub description: String,
    pub max_args: usize,
    pub section: &'static str,
}

impl CommandRow {
    pub fn takes_args(&self) -> bool {
        self.max_args > 0
    }
}

impl PickerItem for CommandRow {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        (!self.description.is_empty()).then_some(&self.description)
    }

    fn section(&self) -> Option<&str> {
        Some(self.section)
    }
}

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "/compact",
        description: "Summarize and compact conversation history",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/new",
        description: "Start a new session",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/help",
        description: "Show keybindings",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/usage",
        description: "Show token usage breakdown",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/queue",
        description: "Inspect and edit queued prompts",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/stash",
        description: "Park the current prompt draft for later",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/stash-pop",
        description: "Restore the most recently stashed prompt",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/stash-list",
        description: "Browse stashed prompts",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/memory",
        description: "View, edit, and delete memory files",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/tasks",
        description: "Browse tasks and steer running subagents",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/sessions",
        description: "Browse and switch sessions",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/rename",
        description: "Rename the current session",
        max_args: usize::MAX,
    },
    BuiltinCommand {
        name: "/model",
        description: "Switch model",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/system-prompt",
        description: "Switch system prompt profile",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/review",
        description: "Review the last reply passage by passage",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/theme",
        description: "Switch color theme",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/view",
        description: "Cycle transcript: auto / compact / expanded",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/mcp",
        description: "Configure MCP servers",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/permissions",
        description: "Inspect active conversation permission rules",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/login",
        description: "Authenticate with an LLM provider",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/cd",
        description: "Change working directory",
        max_args: 1,
    },
    BuiltinCommand {
        name: "/btw",
        description: "Ask a quick question (no tools, no history pollution)",
        max_args: usize::MAX,
    },
    BuiltinCommand {
        name: "/goal",
        description: "Work until a completion condition is met",
        max_args: usize::MAX,
    },
    BuiltinCommand {
        name: "/goal-clear",
        description: "Stop the active completion goal",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/goal-model",
        description: "Choose the completion goal evaluator",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/thinking",
        description: "Set reasoning (off, adaptive/provider default, effort, or token budget)",
        max_args: 1,
    },
    BuiltinCommand {
        name: "/fast",
        description: "Toggle Anthropic fast mode (Opus only)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/workflow",
        description: "Toggle workflow context for custom Lua tools",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/exit",
        description: "Exit the application",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/reload",
        description: "Reload plugins and config",
        max_args: 0,
    },
];

pub struct ParsedCommand {
    pub name: String,
    pub args: String,
}

pub enum CommandAction {
    Consumed,
    Execute(ParsedCommand),
    Complete(String),
    Passthrough,
}

#[derive(Clone)]
enum CommandType {
    Builtin(&'static BuiltinCommand),
    Custom(usize),
    McpPrompt(usize),
    Lua(usize),
}

struct CommandItem {
    name: String,
    max_args: usize,
    command_type: CommandType,
}

struct Match {
    command_type: CommandType,
    indices: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommandRowKey {
    Builtin(&'static str),
    Custom(usize),
    McpPrompt(usize),
    Lua(usize),
}

impl CommandType {
    fn row_key(&self) -> CommandRowKey {
        match self {
            Self::Builtin(command) => CommandRowKey::Builtin(command.name),
            Self::Custom(index) => CommandRowKey::Custom(*index),
            Self::McpPrompt(index) => CommandRowKey::McpPrompt(*index),
            Self::Lua(index) => CommandRowKey::Lua(*index),
        }
    }
}

#[derive(Clone, Copy)]
struct CommandRowHit {
    area: Rect,
    filtered_index: usize,
    key: CommandRowKey,
}

pub struct CommandPalette {
    selected: usize,
    filtered: Vec<Match>,
    custom: Arc<[CustomCommand]>,
    mcp_reader: McpSnapshotReader,
    mcp_prompts: Vec<McpPromptInfo>,
    mcp_generation: u64,
    lua_reader: LuaCommandReader,
    lua_commands: Vec<LuaCommandInfo>,
    lua_generation: u64,
    nucleo: Nucleo<CommandItem>,
    matcher: Matcher,
    current_arg_count: usize,
    /// First visible row. The popup is capped by the space above the input, so
    /// a long list has rows that only scrolling reaches.
    scroll_offset: usize,
    /// Rows the popup last had room for, which only rendering can know.
    viewport_height: usize,
    popup_area: Option<Rect>,
    row_hits: Vec<CommandRowHit>,
    mouse_down: Option<CommandRowKey>,
}

impl CommandPalette {
    pub fn new(
        custom_commands: Arc<[CustomCommand]>,
        mcp_reader: McpSnapshotReader,
        lua_reader: LuaCommandReader,
    ) -> Self {
        let snap = mcp_reader.load();
        let mcp_generation = snap.generation;
        let prompts = snap.prompts.clone();

        let lua_snap = lua_reader.load();
        let lua_generation = lua_snap.generation;
        let lua_commands = lua_snap.commands.clone();

        let nucleo = Self::build_nucleo(&custom_commands, &prompts, &lua_commands);
        Self {
            selected: 0,
            filtered: Vec::new(),
            custom: custom_commands,
            mcp_reader,
            mcp_prompts: prompts,
            mcp_generation,
            lua_reader,
            lua_commands,
            lua_generation,
            nucleo,
            matcher: Matcher::new(Config::DEFAULT),
            current_arg_count: 0,
            scroll_offset: 0,
            viewport_height: 0,
            popup_area: None,
            row_hits: Vec::new(),
            mouse_down: None,
        }
    }

    /// Every command the palette knows, in display order. The one place that
    /// enumerates the four sources: matching and name lookup both read it.
    fn items<'a>(
        custom_commands: &'a [CustomCommand],
        mcp_prompts: &'a [McpPromptInfo],
        lua_commands: &'a [LuaCommandInfo],
    ) -> impl Iterator<Item = CommandItem> + 'a {
        let builtins = BUILTIN_COMMANDS.iter().map(|cmd| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Builtin(cmd),
        });
        let custom = custom_commands
            .iter()
            .enumerate()
            .map(|(i, cmd)| CommandItem {
                name: cmd.display_name(),
                max_args: if cmd.has_args() { usize::MAX } else { 0 },
                command_type: CommandType::Custom(i),
            });
        let prompts = mcp_prompts.iter().enumerate().map(|(i, p)| CommandItem {
            name: format!("/{}", p.display_name),
            max_args: if p.arguments.is_empty() {
                0
            } else {
                usize::MAX
            },
            command_type: CommandType::McpPrompt(i),
        });
        let lua = lua_commands.iter().enumerate().map(|(i, cmd)| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Lua(i),
        });
        builtins.chain(custom).chain(prompts).chain(lua)
    }

    fn build_nucleo(
        custom_commands: &[CustomCommand],
        mcp_prompts: &[McpPromptInfo],
        lua_commands: &[LuaCommandInfo],
    ) -> Nucleo<CommandItem> {
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();

        for item in Self::items(custom_commands, mcp_prompts, lua_commands) {
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        nucleo
    }

    pub fn handle_key(&mut self, key: KeyEvent, input: &str) -> CommandAction {
        if !self.is_active() {
            return CommandAction::Passthrough;
        }
        match key.code {
            KeyCode::Up => {
                self.move_up();
                CommandAction::Consumed
            }
            KeyCode::Down => {
                self.move_down();
                CommandAction::Consumed
            }
            KeyCode::Esc => {
                self.close();
                CommandAction::Consumed
            }
            KeyCode::Enter => self.activate_selected(input),
            KeyCode::Tab => {
                if let Some(item) = self.filtered.get(self.selected) {
                    let name = self.item_name(item);
                    let text = if self.item_has_args(item) {
                        format!("{name} ")
                    } else {
                        name
                    };
                    CommandAction::Complete(text)
                } else {
                    CommandAction::Consumed
                }
            }
            _ => CommandAction::Passthrough,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent, input: &str) -> CommandAction {
        let position = Position::new(event.column, event.row);
        let over_popup = self.popup_area.is_some_and(|area| area.contains(position));

        if matches!(
            event.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        ) && !over_popup
        {
            self.mouse_down = None;
        }
        if !over_popup {
            return CommandAction::Passthrough;
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = None;
                if let Some(hit) = self
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .copied()
                {
                    self.selected = hit.filtered_index;
                    self.mouse_down = Some(hit.key);
                }
                CommandAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_down = None;
                CommandAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(hit) = self.row_hits.iter().find(|hit| hit.area.contains(position)) {
                    self.selected = hit.filtered_index;
                }
                CommandAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed_key) = self.mouse_down.take() else {
                    return CommandAction::Consumed;
                };
                let released = self
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .map(|hit| (hit.filtered_index, hit.key));
                let Some((filtered_index, released_key)) = released else {
                    return CommandAction::Consumed;
                };
                if released_key != pressed_key {
                    return CommandAction::Consumed;
                }
                self.selected = filtered_index;
                self.activate_selected(input)
            }
            _ => CommandAction::Consumed,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.filtered.is_empty()
    }

    /// Pulls MCP prompts and Lua commands forward when either side has
    /// published a new snapshot. Both palettes read the same sources, so
    /// neither may skip this before enumerating.
    fn refresh_sources(&mut self) {
        let mcp_snap = self.mcp_reader.load();
        let lua_snap = self.lua_reader.load();
        if mcp_snap.generation == self.mcp_generation && lua_snap.generation == self.lua_generation
        {
            return;
        }
        self.mcp_generation = mcp_snap.generation;
        self.mcp_prompts = mcp_snap.prompts.clone();
        self.lua_generation = lua_snap.generation;
        self.lua_commands = lua_snap.commands.clone();
        self.nucleo = Self::build_nucleo(&self.custom, &self.mcp_prompts, &self.lua_commands);
    }

    /// Every command as a modal picker row, grouped by source.
    pub fn rows(&mut self) -> Vec<CommandRow> {
        self.refresh_sources();
        Self::items(&self.custom, &self.mcp_prompts, &self.lua_commands)
            .map(|item| CommandRow {
                description: self.describe(&item.command_type).to_string(),
                section: Self::section_of(&item.command_type),
                name: item.name,
                max_args: item.max_args,
            })
            .collect()
    }

    fn section_of(command_type: &CommandType) -> &'static str {
        match command_type {
            CommandType::Builtin(_) => SECTION_BUILTIN,
            CommandType::Custom(_) => SECTION_CUSTOM,
            CommandType::McpPrompt(_) => SECTION_MCP,
            CommandType::Lua(_) => SECTION_PLUGIN,
        }
    }

    fn describe(&self, command_type: &CommandType) -> &str {
        match command_type {
            CommandType::Builtin(cmd) => cmd.description,
            CommandType::Custom(i) => &self.custom[*i].description,
            CommandType::McpPrompt(i) => &self.mcp_prompts[*i].description,
            CommandType::Lua(i) => &self.lua_commands[*i].description,
        }
    }

    pub fn sync(&mut self, input: &str) {
        self.invalidate_mouse_geometry();
        self.refresh_sources();
        let Some(stripped) = input.strip_prefix('/') else {
            self.filtered.clear();
            self.current_arg_count = 0;
            return;
        };

        let parts: Vec<&str> = stripped.split_whitespace().collect();
        let cmd_word = parts.first().copied().unwrap_or(stripped);
        let trailing_space = stripped.ends_with(char::is_whitespace);

        self.current_arg_count = if trailing_space {
            parts.len()
        } else {
            parts.len().saturating_sub(1)
        };

        self.nucleo.pattern.reparse(
            0,
            cmd_word,
            CaseMatching::Ignore,
            Normalization::Smart,
            false,
        );

        self.tick();
    }

    fn tick(&mut self) {
        loop {
            let status = self.nucleo.tick(TICK_TIMEOUT_MS);
            if status.changed {
                self.refresh_matches();
            }
            if !status.running {
                break;
            }
        }
    }

    fn refresh_matches(&mut self) {
        self.invalidate_mouse_geometry();
        let snapshot = self.nucleo.snapshot();
        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();

        self.filtered.clear();
        let count = snapshot.matched_item_count();
        for item in snapshot.matched_items(0..count) {
            let cmd_item = &item.data;
            let col = &item.matcher_columns[0];

            if self.current_arg_count > cmd_item.max_args {
                continue;
            }

            let indices = if has_pattern {
                let mut indices_buf = vec![];
                pattern.column_pattern(0).indices(
                    col.slice(..),
                    &mut self.matcher,
                    &mut indices_buf,
                );
                indices_buf
            } else {
                Vec::new()
            };

            self.filtered.push(Match {
                command_type: cmd_item.command_type.clone(),
                indices,
            });
        }

        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.ensure_visible();
    }

    pub fn close(&mut self) {
        self.filtered.clear();
        self.current_arg_count = 0;
        self.scroll_offset = 0;
        self.invalidate_mouse_geometry();
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
        self.ensure_visible();
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == self.filtered.len() - 1 {
            0
        } else {
            self.selected + 1
        };
        self.ensure_visible();
    }

    /// Brings the selection back into the popup. Only a selection move calls
    /// this: doing it every frame would undo the wheel on the next repaint.
    fn ensure_visible(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        let offset = if self.selected < self.scroll_offset {
            self.selected
        } else if self.selected >= self.scroll_offset + self.viewport_height {
            self.selected + 1 - self.viewport_height
        } else {
            return;
        };
        self.scroll_offset = offset;
        self.invalidate_mouse_geometry();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup_area.is_some_and(|area| area.contains(pos))
    }

    pub fn scroll(&mut self, delta: i32) {
        let max_offset = self.filtered.len().saturating_sub(self.viewport_height);
        let offset = if delta > 0 {
            self.scroll_offset.saturating_sub(delta as usize)
        } else {
            self.scroll_offset
                .saturating_add(delta.unsigned_abs() as usize)
        };
        let offset = offset.min(max_offset);
        if offset != self.scroll_offset {
            self.scroll_offset = offset;
            self.invalidate_mouse_geometry();
        }
    }

    fn item_name(&self, m: &Match) -> String {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.name.to_string(),
            CommandType::Custom(i) => self.custom[*i].display_name(),
            CommandType::McpPrompt(i) => format!("/{}", self.mcp_prompts[*i].display_name),
            CommandType::Lua(i) => self.lua_commands[*i].name.to_string(),
        }
    }

    fn item_has_args(&self, m: &Match) -> bool {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.max_args > 0,
            CommandType::Custom(i) => self.custom[*i].has_args(),
            CommandType::McpPrompt(i) => !self.mcp_prompts[*i].arguments.is_empty(),
            CommandType::Lua(i) => self.lua_commands[*i].max_args > 0,
        }
    }

    fn item_description(&self, m: &Match) -> &str {
        self.describe(&m.command_type)
    }

    pub fn confirm(&self, input: &str) -> Option<ParsedCommand> {
        let item = self.filtered.get(self.selected)?;
        let name = self.item_name(item);
        let args = input
            .strip_prefix('/')
            .and_then(|s| s.split_once(char::is_whitespace))
            .map(|(_, a)| a.trim())
            .unwrap_or("");
        Some(ParsedCommand {
            name,
            args: args.to_string(),
        })
    }

    fn activate_selected(&mut self, input: &str) -> CommandAction {
        match self.confirm(input) {
            Some(cmd) => {
                self.close();
                CommandAction::Execute(cmd)
            }
            None => CommandAction::Consumed,
        }
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.popup_area = None;
        self.row_hits.clear();
        self.mouse_down = None;
    }

    /// Name lookup for `caudra.api.run_command`, returning the registered
    /// spelling that [`crate::app::App`] dispatches on. Case-insensitive like
    /// typing, but never fuzzy: an alias names one command on purpose, and a
    /// typo should report itself instead of running the closest neighbor.
    pub fn resolve(&self, name: &str) -> Option<String> {
        Self::items(&self.custom, &self.mcp_prompts, &self.lua_commands)
            .map(|item| item.name)
            .find(|n| n.eq_ignore_ascii_case(name))
    }

    pub fn find_custom_command(&self, display_name: &str) -> Option<&CustomCommand> {
        self.custom
            .iter()
            .find(|c| c.display_name() == display_name)
    }

    pub fn find_mcp_prompt(&self, slash_name: &str) -> Option<&McpPromptInfo> {
        let name = slash_name.strip_prefix('/')?;
        self.mcp_prompts.iter().find(|p| p.display_name == name)
    }

    pub fn find_lua_command(&self, name: &str) -> Option<&LuaCommandInfo> {
        self.lua_commands.iter().find(|c| c.name.as_ref() == name)
    }

    pub fn view(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        let filtered = &self.filtered;
        if filtered.is_empty() {
            self.invalidate_mouse_geometry();
            return None;
        }

        let popup_height = (filtered.len() as u16).min(input_area.y);
        if popup_height == 0 {
            self.invalidate_mouse_geometry();
            return None;
        }

        const GAP: usize = 2;
        let max_name = filtered
            .iter()
            .map(|item| self.item_name(item).len())
            .max()
            .unwrap_or(0);
        let max_desc = filtered
            .iter()
            .map(|item| self.item_description(item).len())
            .max()
            .unwrap_or(0);
        const PAD: usize = 1;
        let popup_width = (PAD + max_name + GAP + max_desc + PAD) as u16;

        let popup = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width.min(input_area.width),
            height: popup_height,
        };

        let t = theme::current();
        let viewport_height = popup_height as usize;
        let scroll_offset = self
            .scroll_offset
            .min(filtered.len().saturating_sub(viewport_height));
        let end = (scroll_offset + viewport_height).min(filtered.len());
        let lines: Vec<Line> = filtered[scroll_offset..end]
            .iter()
            .enumerate()
            .map(|(row, m)| {
                let i = scroll_offset + row;
                let name = self.item_name(m);
                let desc = self.item_description(m);
                let selected = i == self.selected;
                let name_pad = max_name - name.len() + GAP;

                if selected {
                    let s = t.item_selected;
                    let highlighted_name = self.build_highlighted_spans(&name, &m.indices, s);
                    let mut spans = vec![Span::styled(" ".repeat(PAD), s)];
                    spans.extend(highlighted_name);
                    spans.push(Span::styled(" ".repeat(name_pad), s));
                    spans.push(Span::styled(desc, s));
                    spans.push(Span::styled(" ".repeat(PAD), s));
                    Line::from(spans)
                } else {
                    let highlighted_name = self.build_highlighted_spans(&name, &m.indices, t.item);
                    let mut spans = vec![Span::raw(" ".repeat(PAD))];
                    spans.extend(highlighted_name);
                    spans.push(Span::raw(" ".repeat(name_pad)));
                    spans.push(Span::styled(desc, t.item_desc));
                    spans.push(Span::raw(" ".repeat(PAD)));
                    Line::from(spans)
                }
            })
            .collect();

        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(t.surface_style()), popup);

        self.row_hits = filtered[scroll_offset..end]
            .iter()
            .enumerate()
            .map(|(row, item)| CommandRowHit {
                area: Rect::new(popup.x, popup.y + row as u16, popup.width, 1),
                filtered_index: scroll_offset + row,
                key: item.command_type.row_key(),
            })
            .collect();
        self.popup_area = Some(popup);
        self.viewport_height = viewport_height;
        self.scroll_offset = scroll_offset;

        Some(popup)
    }

    fn build_highlighted_spans(&self, text: &str, indices: &[u32], base: Style) -> Vec<Span<'_>> {
        if indices.is_empty() {
            return vec![Span::styled(text.to_string(), base)];
        }

        let t = theme::current();
        let highlight = base
            .fg(t.accent.fg.unwrap_or(t.foreground))
            .add_modifier(Modifier::BOLD);

        let mut spans = Vec::new();
        let mut in_match = false;
        let mut run = String::new();

        for (i, ch) in text.chars().enumerate() {
            let is_match = indices.binary_search(&(i as u32)).is_ok();
            if is_match != in_match && !run.is_empty() {
                spans.push(Span::styled(
                    mem::take(&mut run),
                    if in_match { highlight } else { base },
                ));
            }
            in_match = is_match;
            run.push(ch);
        }

        if !run.is_empty() {
            spans.push(Span::styled(run, if in_match { highlight } else { base }));
        }

        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::{McpPromptArg, McpSnapshot};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use test_case::test_case;

    fn empty_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::empty()
    }

    fn synced(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render(palette: &mut CommandPalette) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                palette.view(frame, Rect::new(0, 20, 80, 4));
            })
            .unwrap();
    }

    fn synced_with_custom(input: &str, custom: Arc<[CustomCommand]>) -> CommandPalette {
        let mut p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    fn sample_custom() -> Arc<[CustomCommand]> {
        Arc::from([
            CustomCommand {
                name: "review".into(),
                description: "Code review".into(),
                content: "Review $ARGUMENTS".into(),
                scope: caudra_agent::command::CommandScope::Project,
                accepts_args: true,
            },
            CustomCommand {
                name: "fix".into(),
                description: "Quick fix".into(),
                content: "Fix the code".into(),
                scope: caudra_agent::command::CommandScope::User,
                accepts_args: false,
            },
        ])
    }

    #[test]
    fn slash_shows_builtins_plus_extras() {
        let builtin_count = synced("/").filtered.len();
        assert!(builtin_count > 0);

        let with_custom = synced_with_custom("/", sample_custom());
        assert_eq!(with_custom.filtered.len(), builtin_count + 2);

        let with_prompts = synced_with_prompts("/");
        assert_eq!(with_prompts.filtered.len(), builtin_count + 2);
    }

    #[test]
    fn close_deactivates() {
        let mut p = synced("/");
        p.close();
        assert!(!p.is_active());
    }

    #[test_case("/mp", true ; "compact_substring")]
    #[test_case("/ew", true ; "lowercase_substring")]
    #[test_case("/EW", true ; "uppercase_substring")]
    #[test_case("/zzz", false ; "no_match")]
    fn filter_by_substring(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_active(), expect_active);
    }

    #[test_case("/goal-c", "/goal-clear" ; "clear")]
    #[test_case("/goal-m", "/goal-model" ; "model")]
    fn goal_control_commands_are_discoverable(input: &str, expected: &str) {
        let p = synced(input);
        assert!(p.filtered.iter().any(|item| p.item_name(item) == expected));
    }

    #[test]
    fn filter_custom_by_substring() {
        let p = synced_with_custom("/review", sample_custom());
        assert!(p.is_active());
        assert!(
            p.filtered
                .iter()
                .any(|item| matches!(item.command_type, CommandType::Custom(0)))
        );
    }

    #[test]
    fn navigation_wraps() {
        let mut p = synced("/");
        p.move_up();
        assert_eq!(p.selected, p.filtered.len() - 1);
        p.move_down();
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn hovering_command_moves_selection() {
        let mut palette = synced("/");
        render(&mut palette);
        let hit = palette.row_hits[2];

        assert!(matches!(
            palette.handle_mouse(mouse(MouseEventKind::Moved, hit.area), "/"),
            CommandAction::Consumed
        ));
        assert_eq!(palette.selected, hit.filtered_index);
    }

    #[test]
    fn clicking_command_executes_with_enter_semantics() {
        let mut palette = synced("/");
        render(&mut palette);
        let hit = palette.row_hits[1];
        let expected_name = palette.item_name(&palette.filtered[hit.filtered_index]);

        palette.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), hit.area),
            "/",
        );
        let action =
            palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area), "/");

        assert!(matches!(
            action,
            CommandAction::Execute(ParsedCommand { name, args })
                if name == expected_name && args.is_empty()
        ));
        assert!(!palette.is_active());
    }

    #[test]
    fn releasing_on_another_command_does_not_execute() {
        let mut palette = synced("/");
        render(&mut palette);
        let first = palette.row_hits[0];
        let second = palette.row_hits[1];

        palette.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), first.area),
            "/",
        );
        let action = palette.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), second.area),
            "/",
        );

        assert!(matches!(action, CommandAction::Consumed));
        assert!(palette.is_active());
    }

    #[test]
    fn dragging_command_cancels_click() {
        let mut palette = synced("/");
        render(&mut palette);
        let hit = palette.row_hits[0];

        palette.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), hit.area),
            "/",
        );
        palette.handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), hit.area),
            "/",
        );
        let action =
            palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area), "/");

        assert!(matches!(action, CommandAction::Consumed));
        assert!(palette.is_active());
    }

    #[test]
    fn command_filtering_invalidates_rendered_popup() {
        let mut palette = synced("/");
        render(&mut palette);
        let stale = palette.row_hits[0];
        palette.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), stale.area),
            "/",
        );

        palette.sync("/goal");
        let action = palette.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), stale.area),
            "/goal",
        );

        assert!(matches!(action, CommandAction::Passthrough));
        assert!(palette.popup_area.is_none());
        assert!(palette.mouse_down.is_none());
    }

    #[test]
    fn command_mouse_events_outside_popup_pass_through() {
        let mut palette = synced("/");
        render(&mut palette);
        let popup = palette.popup_area.unwrap();
        let outside = Rect::new(popup.right(), popup.y, 1, 1);

        assert!(matches!(
            palette.handle_mouse(mouse(MouseEventKind::Moved, outside), "/"),
            CommandAction::Passthrough
        ));
    }

    #[test]
    fn confirm_when_inactive_returns_none() {
        let p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        assert!(p.confirm("").is_none());
    }

    #[test]
    fn sync_clamps_selected() {
        let mut p = synced("/");
        p.selected = 100;
        p.sync("/");
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn sync_filters_on_first_word_only() {
        let p = synced("/cd ~/foo");
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        let name = p.item_name(&p.filtered[0]);
        assert_eq!(name, "/cd");
    }

    #[test_case("/compact ", false ; "zero_arg_cmd_with_space")]
    #[test_case("/help ", false     ; "zero_arg_help_with_space")]
    #[test_case("/cd ", true        ; "one_arg_cmd_with_space")]
    #[test_case("/cd ~/foo", true   ; "one_arg_cmd_mid_arg")]
    #[test_case("/cd  ~/foo", true  ; "one_arg_cmd_double_space")]
    #[test_case("/cd ~/foo ", false ; "one_arg_cmd_second_space")]
    #[test_case("/btw hello world", true ; "btw_stays_active_with_many_args")]
    fn sync_respects_nargs(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_active(), expect_active);
    }

    #[test]
    fn custom_command_with_args_stays_active() {
        let p = synced_with_custom("/project:review some args", sample_custom());
        assert!(p.is_active());
    }

    #[test]
    fn custom_command_without_args_hides_on_space() {
        let p = synced_with_custom("/user:fix ", sample_custom());
        assert!(!p.is_active());
    }

    #[test_case("/cd", "/cd", ""              ; "no_args")]
    #[test_case("/cd ~/foo", "/cd", "~/foo"   ; "with_args")]
    #[test_case("/CD ~/foo", "/cd", "~/foo"   ; "case_insensitive")]
    #[test_case("/compact", "/compact", ""    ; "other_command")]
    #[test_case("/cmp", "/compact", ""    ; "fuzzy-match-1")]
    #[test_case("/pct", "/compact", ""    ; "fuzzy-match-2")]
    #[test_case("/btw hello world", "/btw", "hello world" ; "btw_multi_word")]
    fn confirm_parses_args(input: &str, expected_name: &str, expected_args: &str) {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.args, expected_args);
    }

    #[test]
    fn confirm_custom_command() {
        let custom = sample_custom();
        let mut p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        p.sync("/project:review");
        assert!(p.is_active());
        let cmd = p.confirm("/project:review some-file.rs").unwrap();
        assert_eq!(cmd.name, "/project:review");
        assert_eq!(cmd.args, "some-file.rs");
    }

    #[test]
    fn find_custom_command_lookup() {
        let custom = sample_custom();
        let p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        let found = p.find_custom_command("/project:review");
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "Review $ARGUMENTS");
        assert!(p.find_custom_command("/nonexistent").is_none());
    }

    fn sample_prompts() -> McpSnapshotReader {
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![],
            prompts: vec![
                McpPromptInfo {
                    display_name: "myserver:code-review".into(),
                    qualified_name: "myserver/code-review".into(),
                    description: "Review code changes".into(),
                    arguments: vec![McpPromptArg {
                        name: "diff".into(),
                        description: "The diff".into(),
                        required: true,
                    }],
                },
                McpPromptInfo {
                    display_name: "myserver:summarize".into(),
                    qualified_name: "myserver/summarize".into(),
                    description: "Summarize text".into(),
                    arguments: vec![],
                },
            ],
            pids: vec![],
            generation: 0,
        })
    }

    fn synced_with_prompts(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), sample_prompts(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    #[test]
    fn filter_mcp_prompt_by_substring() {
        let p = synced_with_prompts("/code");
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(
            p.filtered[0].command_type,
            CommandType::McpPrompt(0)
        ));
    }

    #[test]
    fn mcp_prompt_with_args_stays_active() {
        let p = synced_with_prompts("/myserver:code-review some diff");
        assert!(p.is_active());
    }

    #[test]
    fn mcp_prompt_without_args_hides_on_space() {
        let p = synced_with_prompts("/myserver:summarize ");
        assert!(
            !p.filtered
                .iter()
                .any(|f| matches!(f.command_type, CommandType::McpPrompt(1)))
        );
    }

    #[test]
    fn find_mcp_prompt_lookup() {
        let p = synced_with_prompts("/");
        let found = p.find_mcp_prompt("/myserver:code-review");
        assert!(found.is_some());
        assert_eq!(found.unwrap().qualified_name, "myserver/code-review");
        assert!(p.find_mcp_prompt("/nonexistent").is_none());
    }

    #[test]
    fn confirm_mcp_prompt_parses_args() {
        let input = "/myserver:code-review my-diff-content";
        let mut p = synced_with_prompts(input);
        p.selected = p
            .filtered
            .iter()
            .position(|f| matches!(f.command_type, CommandType::McpPrompt(0)))
            .unwrap();
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, "/myserver:code-review");
        assert_eq!(cmd.args, "my-diff-content");
    }

    #[test]
    fn mcp_update_clears_old_prompts() {
        let reader = sample_prompts();
        let mut p = CommandPalette::new(Arc::from([]), reader, LuaCommandReader::empty());

        p.sync("/");
        let initial_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .count();
        assert_eq!(initial_count, 2, "Should have 2 MCP prompts initially");

        let updated_reader = McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![],
            prompts: vec![McpPromptInfo {
                display_name: "myserver:new-prompt".into(),
                qualified_name: "myserver/new-prompt".into(),
                description: "A new prompt".into(),
                arguments: vec![],
            }],
            pids: vec![],
            generation: 1,
        });

        p.mcp_reader = updated_reader;
        p.sync("/");

        let updated_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .count();
        assert_eq!(
            updated_count, 1,
            "Should have only 1 MCP prompt after update"
        );

        assert!(!p.filtered.is_empty(), "Should have filtered results");
        let prompt = &p
            .filtered
            .iter()
            .find(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .expect("Should have at least one MCP prompt");
        match &prompt.command_type {
            CommandType::McpPrompt(i) => {
                assert_eq!(p.mcp_prompts[*i].display_name, "myserver:new-prompt");
            }
            _ => panic!("Should have MCP prompt"),
        }
    }

    #[test_case("/cmp", "/compact" ; "compact_fuzzy")]
    #[test_case("/new", "/new" ; "new_exact")]
    #[test_case("/thm", "/theme" ; "theme_fuzzy")]
    fn nucleo_highlights_matching_indices(input: &str, expected_cmd: &str) {
        let p = synced(input);
        assert!(p.is_active(), "Input '{}' should activate palette", input);
        // Find the expected match
        let matched = p
            .filtered
            .iter()
            .find(|m| p.item_name(m) == expected_cmd)
            .unwrap_or_else(|| panic!("Should find {} for input {}", expected_cmd, input));
        // Should have some highlight indices
        assert!(
            !matched.indices.is_empty(),
            "Match should have highlight indices"
        );
    }

    fn sample_lua_commands() -> LuaCommandReader {
        LuaCommandReader::from_commands(vec![
            LuaCommandInfo {
                name: Arc::from("/memory"),
                description: Arc::from("View memory files"),
                plugin: Arc::from("memory"),
                max_args: 0,
            },
            LuaCommandInfo {
                name: Arc::from("/deploy"),
                description: Arc::from("Deploy the project"),
                plugin: Arc::from("deploy_plugin"),
                max_args: 0,
            },
        ])
    }

    fn synced_with_nargs(input: &str, max_args: usize) -> CommandPalette {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from("/deploy"),
            description: Arc::from("Deploy the project"),
            plugin: Arc::from("deploy_plugin"),
            max_args,
        }]);
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), reader);
        p.sync(input);
        p
    }

    #[test_case("/deploy", usize::MAX, true           ; "nargs_plus_no_args")]
    #[test_case("/deploy ", usize::MAX, true          ; "nargs_plus_trailing_space")]
    #[test_case("/deploy my target", usize::MAX, true ; "nargs_plus_multi_word")]
    #[test_case("/deploy target", 1, true             ; "nargs_one_single_word")]
    #[test_case("/deploy my target", 1, false         ; "nargs_one_too_many")]
    #[test_case("/deploy", 0, true                    ; "nargs_zero_no_args")]
    #[test_case("/deploy target", 0, false            ; "nargs_zero_with_arg")]
    fn lua_command_respects_nargs(input: &str, max_args: usize, expect_active: bool) {
        assert_eq!(
            synced_with_nargs(input, max_args).is_active(),
            expect_active
        );
    }

    #[test]
    fn confirm_lua_command_keeps_multi_word_args() {
        let input = "/deploy my new target";
        let cmd = synced_with_nargs(input, usize::MAX).confirm(input).unwrap();
        assert_eq!(cmd.name, "/deploy");
        assert_eq!(cmd.args, "my new target");
    }

    fn synced_with_lua(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), sample_lua_commands());
        p.sync(input);
        p
    }

    #[test]
    fn lua_commands_appear_in_unfiltered_list() {
        let p = synced_with_lua("/");
        let lua_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(lua_count, 2);
    }

    #[test]
    fn lua_command_filtered_by_substring() {
        let p = synced_with_lua("/mem");
        assert!(p.is_active());
        let found = p
            .filtered
            .iter()
            .any(|f| matches!(f.command_type, CommandType::Lua(_)) && p.item_name(f) == "/memory");
        assert!(found);
    }

    #[test]
    fn find_lua_command_returns_matching_entry() {
        let p = synced_with_lua("/");
        let found = p.find_lua_command("/memory");
        assert!(found.is_some());
        assert_eq!(found.unwrap().plugin.as_ref(), "memory");
        assert!(p.find_lua_command("/nonexistent").is_none());
    }

    #[test]
    fn confirm_lua_command_parses_args() {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), sample_lua_commands());
        p.sync("/memory");
        let cmd = p.confirm("/memory some-arg").unwrap();
        assert_eq!(cmd.name, "/memory");
        assert_eq!(cmd.args, "some-arg");
    }

    #[test]
    fn lua_commands_update_on_generation_change() {
        let (writer, reader) = caudra_lua::test_support::lua_command_writer_pair();
        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/old"),
            description: Arc::from("old command"),
            plugin: Arc::from("p"),
            max_args: 0,
        }]);
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), reader);
        p.sync("/");
        let initial_lua = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(initial_lua, 1);

        writer.publish(vec![
            LuaCommandInfo {
                name: Arc::from("/new1"),
                description: Arc::from("new"),
                plugin: Arc::from("p"),
                max_args: 0,
            },
            LuaCommandInfo {
                name: Arc::from("/new2"),
                description: Arc::from("new2"),
                plugin: Arc::from("p"),
                max_args: 0,
            },
        ]);
        p.sync("/");
        let updated_lua = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(updated_lua, 2);
        assert!(p.find_lua_command("/old").is_none());
        assert!(p.find_lua_command("/new1").is_some());
    }

    /// Rows above the input are all the popup gets, so a list longer than that
    /// has some only scrolling can reach.
    const CRAMPED_ROWS: u16 = 4;
    const EXPECT_LAST_ROW: &str = "the last command has to be reachable";

    fn render_cramped(palette: &mut CommandPalette) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                palette.view(frame, Rect::new(0, CRAMPED_ROWS, 80, 1));
            })
            .unwrap();
    }

    fn visible(palette: &CommandPalette) -> Vec<usize> {
        palette
            .row_hits
            .iter()
            .map(|hit| hit.filtered_index)
            .collect()
    }

    /// Walking off the bottom used to leave the selection drawn nowhere: the
    /// popup always rendered from the first match.
    #[test]
    fn walking_past_the_fold_brings_the_selection_with_it() {
        let mut palette = synced("/");
        render_cramped(&mut palette);
        let last = palette.filtered.len() - 1;
        assert!(
            last >= usize::from(CRAMPED_ROWS),
            "the list has to overflow"
        );
        for _ in 0..last {
            palette.move_down();
            render_cramped(&mut palette);
        }
        assert_eq!(palette.selected, last);
        assert!(visible(&palette).contains(&last), "{EXPECT_LAST_ROW}");
    }

    #[test]
    fn the_wheel_reaches_rows_past_the_fold() {
        let mut palette = synced("/");
        render_cramped(&mut palette);
        let last = palette.filtered.len() - 1;
        palette.scroll(-(last as i32));
        render_cramped(&mut palette);
        assert!(visible(&palette).contains(&last), "{EXPECT_LAST_ROW}");
        assert_eq!(
            palette.selected, 0,
            "the wheel moves the view, not the pick"
        );
    }

    #[test]
    fn the_wheel_scrolls_up_on_a_positive_delta() {
        let mut palette = synced("/");
        render_cramped(&mut palette);
        palette.scroll(-2);
        render_cramped(&mut palette);
        assert_eq!(palette.scroll_offset, 2);
        palette.scroll(2);
        render_cramped(&mut palette);
        assert_eq!(palette.scroll_offset, 0);
    }

    /// A row hit has to name the command drawn on it, or a click after
    /// scrolling runs the wrong one.
    #[test]
    fn a_scrolled_row_hit_names_the_command_drawn_on_it() {
        let mut palette = synced("/");
        render_cramped(&mut palette);
        palette.scroll(-3);
        render_cramped(&mut palette);
        let hit = *palette.row_hits.first().expect("a row was drawn");
        assert_eq!(hit.filtered_index, palette.scroll_offset);
        assert_eq!(
            hit.key,
            palette.filtered[hit.filtered_index].command_type.row_key()
        );
    }

    #[test]
    fn the_palette_claims_the_wheel_only_where_it_drew() {
        let mut palette = synced("/");
        render_cramped(&mut palette);
        let popup = palette.popup_area.expect("the palette drew");
        assert!(palette.contains(Position::new(popup.x, popup.y)));
        assert!(!palette.contains(Position::new(popup.x, popup.bottom())));
    }
}
