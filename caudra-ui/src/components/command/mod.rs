use std::cmp::Reverse;
use std::iter;
use std::mem;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::command::CustomCommand;
use caudra_agent::mcp::CompletionTarget;
use caudra_agent::{McpPromptInfo, McpSnapshotReader};
use caudra_config::FeatureFlags;
use caudra_grab::grab_scope;
use caudra_lua::{CompletionRequest, EventHandle, LuaCommandInfo, LuaCommandReader, LuaCompletion};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use smol::Task;

use crate::components::list_picker::{DISABLED_DIM, PickerItem};
use crate::components::match_spans;
use crate::components::mcp_completion::{AsyncAsk, CompletionAsk, complete};
use crate::input_document::char_to_byte;
use crate::repaint::Dirty;
use crate::theme;

mod builtins;
pub(crate) mod grammar;
pub(crate) mod suggest;
pub(crate) mod words;

pub(crate) use builtins::SEARCH_COMMAND;
pub use builtins::{BUILTIN_COMMANDS, BuiltinCommand, disabled_feature};
use grammar::{Arg, Node, SIGNATURE_WIDTH, Source, Subcommand, truncate};
pub(crate) use suggest::SourceValue;
use suggest::{Arguments, Suggestion, list_paths};

const TICK_TIMEOUT_MS: u64 = 10;
/// Abbreviations that keep opening the command they opened before a newer,
/// shorter name tied them: nucleo breaks a score tie towards the shorter name.
const HELD_ABBREVIATIONS: &[(&str, &str)] = &[("p", "/permissions"), ("f", "/fast")];
pub(crate) const SECTION_BUILTIN: &str = "Built-in";
const SECTION_CUSTOM: &str = "Project & User";
const SECTION_MCP: &str = "MCP Prompts";
const SECTION_PLUGIN: &str = "Plugins";
const PAD: usize = 1;
const GAP: usize = 2;
/// A subcommand row is matched as though a dash joined it to its command,
/// since a space would end the command word: `/goal-c` reaches `/goal clear`.
const SUBCOMMAND_DASH: &str = "-";
const WORD_SEPARATOR: &str = " ";
/// What a custom command's arguments are called when its frontmatter gives
/// no hint, and what a plugin command's are.
const CUSTOM_ARG: &str = "arguments";
const LUA_ARG: &str = "args";
const HINT_BRACKETS: [char; 4] = ['<', '>', '[', ']'];
/// How long a plugin's `complete` function may take before the palette
/// stops waiting for it.
const LUA_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// Which chat a command can act on. [`ChatScope::MainOnly`] reaches the main
/// session's turn or history, which a focused task has no equivalent of, so
/// those stay visible but inert while a task owns the composer. Distinct from
/// [`caudra_agent::command::CommandScope`], which says where a custom command
/// was defined.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChatScope {
    Any,
    MainOnly,
}

/// A command as the modal palette sees it: flat, owned, and independent of
/// the index-based [`CommandType`] the inline dropdown matches against.
#[derive(Clone)]
pub struct CommandRow {
    pub name: String,
    /// What a query is matched against: the name, a subcommand joined to
    /// its command by a dash as the inline dropdown matches it.
    pub search: String,
    pub description: String,
    /// What follows the name, such as `[pop|list]`; empty for none.
    pub signature: String,
    pub max_args: usize,
    pub section: &'static str,
    pub disabled: bool,
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

    fn search_text(&self) -> &str {
        &self.search
    }

    fn suffix(&self) -> Option<&str> {
        (!self.signature.is_empty()).then_some(&self.signature)
    }

    fn detail(&self) -> Option<&str> {
        (!self.description.is_empty()).then_some(&self.description)
    }

    fn section(&self) -> Option<&str> {
        Some(self.section)
    }

    fn is_disabled(&self) -> bool {
        self.disabled
    }
}

pub struct ParsedCommand {
    pub name: String,
    pub args: String,
}

impl ParsedCommand {
    /// The command a composer line runs: its first word, and the rest as
    /// arguments.
    pub fn from_line(line: &str) -> Self {
        let line = line.trim_start();
        let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        Self {
            name: name.to_owned(),
            args: args.trim().to_owned(),
        }
    }
}

/// An edit the palette asks of the composer: `text` over `range`, in chars of
/// the palette text it was synced with, and the caret `caret` chars past the
/// range's start. Everything outside the range stays as the reader left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub range: Range<usize>,
    pub text: String,
    pub caret: usize,
}

impl Completion {
    /// `input`, the text it was synced with, with this edit made.
    pub fn applied_to(&self, input: &str) -> String {
        let start = char_to_byte(input, self.range.start);
        let end = char_to_byte(input, self.range.end);
        format!("{}{}{}", &input[..start], self.text, &input[end..])
    }
}

pub enum CommandAction {
    Consumed,
    /// Complete the command, then run what the composer then holds.
    Run(Completion),
    Complete(Completion),
    /// Nothing the slot under the caret accepts was typed: say how the
    /// command is used, and keep the draft.
    Usage(String),
    Passthrough,
}

#[derive(Clone)]
enum CommandType {
    Builtin(&'static BuiltinCommand),
    /// A subcommand indexed as its own row, such as `/goal clear`.
    Subcommand(&'static BuiltinCommand, &'static Subcommand<'static>),
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
    Subcommand(&'static str, &'static str),
    Custom(usize),
    McpPrompt(usize),
    Lua(usize),
    Suggestion(usize),
}

impl CommandType {
    fn row_key(&self) -> CommandRowKey {
        match self {
            Self::Builtin(command) => CommandRowKey::Builtin(command.name),
            Self::Subcommand(command, sub) => CommandRowKey::Subcommand(command.name, sub.name),
            Self::Custom(index) => CommandRowKey::Custom(*index),
            Self::McpPrompt(index) => CommandRowKey::McpPrompt(*index),
            Self::Lua(index) => CommandRowKey::Lua(*index),
        }
    }
}

#[derive(Clone, Copy)]
struct CommandRowHit {
    area: Rect,
    row_index: usize,
    key: CommandRowKey,
}

/// A question answered off the event loop for the slot under the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaletteAsk {
    Prompt(CompletionAsk),
    Paths {
        base: PathBuf,
        home: Option<PathBuf>,
        token: String,
        directories_only: bool,
    },
    Lua(CompletionRequest),
}

impl PaletteAsk {
    /// Whether an answer to `self` still belongs to the slot `other` asks
    /// about, though what is typed there has moved on.
    fn same_slot(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Prompt(left), Self::Prompt(right)) => {
                left.target == right.target && left.argument == right.argument
            }
            (Self::Lua(left), Self::Lua(right)) => {
                left.plugin == right.plugin && left.command == right.command
            }
            (
                Self::Paths {
                    base: left_base,
                    directories_only: left_only,
                    ..
                },
                Self::Paths {
                    base: right_base,
                    directories_only: right_only,
                    ..
                },
            ) => left_base == right_base && left_only == right_only,
            _ => false,
        }
    }

    fn start(
        self,
        reader: &McpSnapshotReader,
        lua: &EventHandle,
    ) -> Task<Option<Vec<SourceValue>>> {
        match self {
            Self::Lua(request) => {
                let lua = lua.clone();
                smol::spawn(async move {
                    let answer = async { Some(lua.complete_command(request).await) };
                    let lull = async {
                        smol::Timer::after(LUA_COMPLETION_TIMEOUT).await;
                        None
                    };
                    let values = smol::future::or(answer, lull).await?;
                    Some(
                        values
                            .into_iter()
                            .map(|value| SourceValue::new(value, ""))
                            .collect(),
                    )
                })
            }
            Self::Prompt(ask) => {
                let answer = complete(reader, ask);
                smol::spawn(async move {
                    answer.await.map(|values| {
                        values
                            .into_iter()
                            .map(|value| SourceValue::new(value, ""))
                            .collect()
                    })
                })
            }
            Self::Paths {
                base,
                home,
                token,
                directories_only,
            } => smol::spawn(async move {
                Some(
                    smol::unblock(move || {
                        list_paths(&base, home.as_deref(), &token, directories_only)
                    })
                    .await,
                )
            }),
        }
    }
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
    /// Where a plugin command's `complete` function is asked.
    lua_handle: EventHandle,
    features: FeatureFlags,
    nucleo: Nucleo<CommandItem>,
    matcher: Matcher,
    /// The palette text last synced, which every completion is measured in.
    input: String,
    /// Where the caret was in [`Self::input`], in bytes.
    caret: usize,
    /// Where the command word being typed ends in [`Self::input`], in bytes:
    /// at the first space, or at the caret when it sits inside the word.
    token_end: usize,
    current_arg_count: usize,
    /// First visible row. The popup is capped by the space above the input, so
    /// a long list has rows that only scrolling reaches.
    scroll_offset: usize,
    /// Rows the popup last had room for, which only rendering can know.
    viewport_height: usize,
    popup_area: Option<Rect>,
    row_hits: Vec<CommandRowHit>,
    mouse_down: Option<CommandRowKey>,
    /// Whether a task owns the composer, which decides who renders dimmed.
    /// Set by [`CommandPalette::sync`] so it cannot go stale behind the rows
    /// it styles.
    task_focused: bool,
    /// The slot under the caret, once the command word names a command and
    /// the caret has moved past it. While there is one, its suggestions are
    /// the rows.
    arguments: Option<Arguments>,
    /// What the app last offered for a source it answers itself.
    supplied: Option<(Source, Vec<SourceValue>)>,
    /// What the last asynchronous answer offered, and for which ask.
    answered: Option<(PaletteAsk, Vec<SourceValue>)>,
    asking: AsyncAsk<PaletteAsk, Vec<SourceValue>>,
    /// Where relative paths are read from: `None` for a remote workspace,
    /// whose directories this machine cannot list.
    working_directory: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl CommandPalette {
    pub fn new(
        custom_commands: Arc<[CustomCommand]>,
        mcp_reader: McpSnapshotReader,
        lua_reader: LuaCommandReader,
        features: FeatureFlags,
    ) -> Self {
        let snap = mcp_reader.load();
        let mcp_generation = snap.generation;
        let prompts = snap.prompts.clone();

        let lua_snap = lua_reader.load();
        let lua_generation = lua_snap.generation;
        let lua_commands = lua_snap.commands.clone();

        let nucleo = Self::build_nucleo(features, &custom_commands, &prompts, &lua_commands);
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
            lua_handle: EventHandle::inert(),
            features,
            nucleo,
            matcher: Matcher::new(Config::DEFAULT),
            input: String::new(),
            caret: 0,
            token_end: 0,
            current_arg_count: 0,
            scroll_offset: 0,
            viewport_height: 0,
            popup_area: None,
            row_hits: Vec::new(),
            mouse_down: None,
            task_focused: false,
            arguments: None,
            supplied: None,
            answered: None,
            asking: AsyncAsk::default(),
            working_directory: None,
            home: caudra_storage::paths::home(),
        }
    }

    pub(crate) fn with_lua_handle(mut self, handle: EventHandle) -> Self {
        self.lua_handle = handle;
        self
    }

    pub(crate) fn set_custom_commands(&mut self, commands: Arc<[CustomCommand]>) {
        self.custom = commands;
        self.reindex();
    }

    /// Adopts a restarted MCP manager's reader. Its generations count from
    /// zero again, so its prompts are read now rather than left to a
    /// generation check that could mistake it for the manager it replaced.
    pub(crate) fn set_mcp_reader(&mut self, reader: McpSnapshotReader) {
        self.mcp_reader = reader;
        let snapshot = self.mcp_reader.load_full();
        self.mcp_generation = snapshot.generation;
        self.mcp_prompts = snapshot.prompts.clone();
        self.reindex();
    }

    /// Where a typed relative path starts, or `None` when the session works
    /// somewhere this machine cannot list.
    pub(crate) fn set_working_directory(&mut self, directory: Option<&Path>) {
        if self.working_directory.as_deref() != directory {
            self.working_directory = directory.map(Path::to_path_buf);
        }
    }

    fn reindex(&mut self) {
        self.nucleo = Self::build_nucleo(
            self.features,
            &self.custom,
            &self.mcp_prompts,
            &self.lua_commands,
        );
        self.filtered.clear();
        self.selected = 0;
    }

    /// Every command the palette knows, in display order. The one place that
    /// enumerates the four sources: matching and name lookup both read it, so
    /// a built-in whose experiment is off is neither suggested nor resolved.
    fn items<'a>(
        features: FeatureFlags,
        custom_commands: &'a [CustomCommand],
        mcp_prompts: &'a [McpPromptInfo],
        lua_commands: &'a [LuaCommandInfo],
    ) -> impl Iterator<Item = CommandItem> + 'a {
        let builtins = BUILTIN_COMMANDS
            .iter()
            .filter(move |cmd| cmd.available(features))
            .map(|cmd| CommandItem {
                name: cmd.name.to_string(),
                max_args: cmd.max_args(),
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

    /// The command followed by its subcommands, each a row of its own.
    fn with_subcommands(item: CommandItem) -> impl Iterator<Item = CommandItem> {
        let subcommands = Self::subcommands_of(&item);
        iter::once(item).chain(subcommands)
    }

    fn subcommands_of(item: &CommandItem) -> impl Iterator<Item = CommandItem> + use<> {
        let command = match item.command_type {
            CommandType::Builtin(command) => Some(command),
            _ => None,
        };
        command.into_iter().flat_map(|command| {
            command.args.subcommands.iter().map(move |sub| CommandItem {
                name: format!("{}{WORD_SEPARATOR}{}", command.name, sub.name),
                max_args: sub.node.max_args(),
                command_type: CommandType::Subcommand(command, sub),
            })
        })
    }

    /// The name a query matches: a subcommand row joined by a dash, since a
    /// space would end the command word.
    fn search_name(item: &CommandItem) -> String {
        match item.command_type {
            CommandType::Subcommand(..) => item.name.replace(WORD_SEPARATOR, SUBCOMMAND_DASH),
            _ => item.name.clone(),
        }
    }

    fn build_nucleo(
        features: FeatureFlags,
        custom_commands: &[CustomCommand],
        mcp_prompts: &[McpPromptInfo],
        lua_commands: &[LuaCommandInfo],
    ) -> Nucleo<CommandItem> {
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();

        for item in Self::items(features, custom_commands, mcp_prompts, lua_commands)
            .flat_map(Self::with_subcommands)
        {
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(Self::search_name(item));
            });
        }

        nucleo
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CommandAction {
        if !self.is_active() {
            return CommandAction::Passthrough;
        }
        let rows = self.row_count();
        match key.code {
            KeyCode::Up if rows > 0 => {
                self.move_up();
                CommandAction::Consumed
            }
            KeyCode::Down if rows > 0 => {
                self.move_down();
                CommandAction::Consumed
            }
            KeyCode::Esc => {
                self.close();
                CommandAction::Consumed
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Tab => self
                .completion()
                .map_or(CommandAction::Consumed, CommandAction::Complete),
            _ => CommandAction::Passthrough,
        }
    }

    /// What Tab takes: the selected suggestion over the word under the caret.
    fn completion(&self) -> Option<Completion> {
        match &self.arguments {
            Some(arguments) => {
                Some(self.take(arguments, arguments.suggestions.get(self.selected)?))
            }
            None => self.word_completion(),
        }
    }

    /// The selected command spliced over the word being typed. A space goes
    /// between it and whatever follows unless one is already there, so a
    /// command put in front of a written prompt takes the prompt as its
    /// arguments, and the caret lands where those arguments start.
    fn word_completion(&self) -> Option<Completion> {
        let item = self.filtered.get(self.selected)?;
        let name = self.item_name(item);
        let tail = &self.input[self.token_end..];
        let takes_rest = self.item_has_args(item) || !tail.is_empty();
        Some(self.splice(0..self.token_end, &name, takes_rest))
    }

    /// `suggestion` over the word under the caret, a space after it when it
    /// leaves more to type.
    fn take(&self, arguments: &Arguments, suggestion: &Suggestion) -> Completion {
        self.splice(
            arguments.token.clone(),
            &suggestion.value,
            suggestion.continues,
        )
    }

    fn splice(&self, bytes: Range<usize>, text: &str, continues: bool) -> Completion {
        let tail = &self.input[bytes.end..];
        let separator = match continues && !tail.starts_with(char::is_whitespace) {
            true => WORD_SEPARATOR,
            false => "",
        };
        let start = self.input[..bytes.start].chars().count();
        Completion {
            range: start..start + self.input[bytes].chars().count(),
            caret: text.chars().count() + usize::from(continues),
            text: format!("{text}{separator}"),
        }
    }

    /// The line exactly as it is.
    fn as_typed(&self) -> Completion {
        let end = self.input.chars().count();
        Completion {
            range: end..end,
            text: String::new(),
            caret: 0,
        }
    }

    /// What Enter does. A slot that takes only listed words runs what was
    /// typed when it is one of them, the highlighted suggestion when it is
    /// not, and nothing at all when nothing matches. Anything else runs as
    /// typed.
    fn enter(&self) -> CommandAction {
        let Some(arguments) = &self.arguments else {
            return self
                .word_completion()
                .map_or(CommandAction::Consumed, CommandAction::Run);
        };
        let token = self.input[arguments.token.clone()].trim();
        if token.is_empty() || !arguments.closed {
            return CommandAction::Run(self.as_typed());
        }
        if let Some(exact) = arguments.exact(token) {
            return CommandAction::Run(match exact.value == token {
                true => self.as_typed(),
                false => self.take(arguments, exact),
            });
        }
        if arguments.accepts_unlisted(token) {
            return CommandAction::Run(self.as_typed());
        }
        match arguments.suggestions.get(self.selected) {
            Some(suggestion) => CommandAction::Run(self.take(arguments, suggestion)),
            None => CommandAction::Usage(arguments.usage()),
        }
    }

    fn activate(&mut self) -> CommandAction {
        let action = self.enter();
        if matches!(action, CommandAction::Run(_)) {
            self.close();
        }
        action
    }

    /// A click runs a command row, as Enter does, and takes a suggestion, as
    /// Tab does.
    fn click(&mut self) -> CommandAction {
        match self.arguments {
            Some(_) => self
                .completion()
                .map_or(CommandAction::Consumed, CommandAction::Complete),
            None => self.activate(),
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> CommandAction {
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
                if let Some(hit) = self.hit(position) {
                    self.selected = hit.row_index;
                    self.mouse_down = Some(hit.key);
                }
                CommandAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_down = None;
                CommandAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(hit) = self.hit(position) {
                    self.selected = hit.row_index;
                }
                CommandAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed_key) = self.mouse_down.take() else {
                    return CommandAction::Consumed;
                };
                let Some(released) = self.hit(position) else {
                    return CommandAction::Consumed;
                };
                if released.key != pressed_key {
                    return CommandAction::Consumed;
                }
                self.selected = released.row_index;
                self.click()
            }
            _ => CommandAction::Consumed,
        }
    }

    fn hit(&self, position: Position) -> Option<CommandRowHit> {
        self.row_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied()
    }

    /// While the line names a command, Enter goes through the palette even
    /// when there is nothing to suggest, so the command runs.
    pub fn is_active(&self) -> bool {
        !self.filtered.is_empty() || self.arguments.is_some()
    }

    /// Rows the selection can move over.
    fn row_count(&self) -> usize {
        match &self.arguments {
            Some(arguments) => arguments.suggestions.len(),
            None => self.filtered.len(),
        }
    }

    /// Dim text that would follow the caret: the rest of the selected
    /// suggestion when the word typed is its start, or the name of an empty
    /// slot. Only offered while the caret ends the line.
    pub fn ghost(&self) -> Option<String> {
        if !self.is_active() || self.caret != self.input.len() || self.input.contains('\n') {
            return None;
        }
        match &self.arguments {
            Some(arguments) => {
                let token = &self.input[arguments.token.clone()];
                if token.is_empty() {
                    return arguments.hint.clone();
                }
                rest_of(&arguments.suggestions.get(self.selected)?.value, token)
            }
            None => rest_of(
                &self.item_name(self.filtered.get(self.selected)?),
                &self.input[..self.token_end],
            ),
        }
    }

    /// Takes an answer for the slot still under the caret.
    pub(crate) fn poll_completion(&mut self) -> Dirty {
        self.poll_completion_at(Instant::now())
    }

    fn poll_completion_at(&mut self, now: Instant) -> Dirty {
        let (reader, lua) = (&self.mcp_reader, &self.lua_handle);
        let Some(answer) = self.asking.poll(now, |ask| ask.start(reader, lua)) else {
            return Dirty::NO;
        };
        self.answered = Some(answer);
        self.refresh_arguments();
        Dirty::YES
    }

    /// Hands over what a source the app answers offers for the slot under
    /// the caret, after [`Self::sync`] asked for it.
    pub(crate) fn provide(&mut self, source: Source, values: Vec<SourceValue>) {
        self.supplied = Some((source, values));
        self.refresh_arguments();
    }

    /// The source the slot under the caret reads, while nobody has offered
    /// its values yet.
    pub(crate) fn wanted(&self) -> Option<Source> {
        let source = self.arguments.as_ref()?.source?;
        let offered = self
            .supplied
            .as_ref()
            .is_some_and(|(supplied, _)| *supplied == source);
        (!source.is_async() && !offered).then_some(source)
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
        self.nucleo = Self::build_nucleo(
            self.features,
            &self.custom,
            &self.mcp_prompts,
            &self.lua_commands,
        );
    }

    /// Every command and subcommand as a modal picker row, grouped by
    /// source. Subcommands follow every built-in, so a command wins a tie
    /// with a verb that matches a query as well. {task_focused} only decides
    /// styling: refusal stays with the one dispatcher.
    pub fn rows(&mut self, task_focused: bool) -> Vec<CommandRow> {
        self.refresh_sources();
        let mut items: Vec<CommandItem> = Self::items(
            self.features,
            &self.custom,
            &self.mcp_prompts,
            &self.lua_commands,
        )
        .collect();
        let builtins = items
            .iter()
            .take_while(|item| matches!(item.command_type, CommandType::Builtin(_)))
            .count();
        let subcommands: Vec<CommandItem> = items[..builtins]
            .iter()
            .flat_map(Self::subcommands_of)
            .collect();
        items.splice(builtins..builtins, subcommands);
        items
            .into_iter()
            .map(|item| CommandRow {
                search: Self::search_name(&item),
                description: self.describe(&item.command_type).to_string(),
                signature: truncate(&self.signature_of(&item.command_type), SIGNATURE_WIDTH),
                section: Self::section_of(&item.command_type),
                disabled: task_focused && Self::scope_of(&item.command_type) == ChatScope::MainOnly,
                name: item.name,
                max_args: item.max_args,
            })
            .collect()
    }

    fn section_of(command_type: &CommandType) -> &'static str {
        match command_type {
            CommandType::Builtin(_) | CommandType::Subcommand(..) => SECTION_BUILTIN,
            CommandType::Custom(_) => SECTION_CUSTOM,
            CommandType::McpPrompt(_) => SECTION_MCP,
            CommandType::Lua(_) => SECTION_PLUGIN,
        }
    }

    /// MCP prompts are main-only by construction rather than by annotation:
    /// `AgentInput::prompt` is expanded in the main agent loop, and a steer
    /// reaches a subagent that never reads it.
    fn scope_of(command_type: &CommandType) -> ChatScope {
        match command_type {
            CommandType::Builtin(command) => command.scope,
            CommandType::Subcommand(command, sub) => sub.scope.unwrap_or(command.scope),
            CommandType::McpPrompt(_) => ChatScope::MainOnly,
            CommandType::Custom(_) | CommandType::Lua(_) => ChatScope::Any,
        }
    }

    /// The styling question the palettes ask, and the refusal question
    /// [`crate::app::App`] asks, answered from one table. A subcommand named
    /// by the leading words of `args` overrides its command's scope.
    pub fn is_main_only(&self, name: &str, args: &str) -> bool {
        self.command_named(name).is_some_and(|command_type| {
            let scope = match &command_type {
                CommandType::Builtin(command) => grammar::scope_of(command.args, args),
                _ => None,
            };
            scope.unwrap_or_else(|| Self::scope_of(&command_type)) == ChatScope::MainOnly
        })
    }

    /// The command `name` names exactly, ignoring case.
    fn command_named(&self, name: &str) -> Option<CommandType> {
        Self::items(
            self.features,
            &self.custom,
            &self.mcp_prompts,
            &self.lua_commands,
        )
        .find(|item| item.name.eq_ignore_ascii_case(name))
        .map(|item| item.command_type)
    }

    fn describe(&self, command_type: &CommandType) -> &str {
        match command_type {
            CommandType::Builtin(cmd) => cmd.description,
            CommandType::Subcommand(_, sub) => sub.description,
            CommandType::Custom(i) => &self.custom[*i].description,
            CommandType::McpPrompt(i) => &self.mcp_prompts[*i].description,
            CommandType::Lua(i) => &self.lua_commands[*i].description,
        }
    }

    fn signature_of(&self, command_type: &CommandType) -> String {
        self.with_node(command_type, |node| node.signature())
    }

    /// The grammar of what follows a command's name. A built-in declares
    /// its own; the rest are read off what their sources say they take.
    fn with_node<R>(&self, command_type: &CommandType, read: impl FnOnce(Node<'_>) -> R) -> R {
        match command_type {
            CommandType::Builtin(command) => read(command.args),
            CommandType::Subcommand(_, sub) => read(sub.node),
            CommandType::Custom(index) => {
                let command = &self.custom[*index];
                let name = command
                    .argument_hint
                    .as_deref()
                    .map_or(CUSTOM_ARG, hint_name);
                let args = [Arg::text(name, "", None)];
                read(Node::args(match command.has_args() {
                    true => &args,
                    false => &[],
                }))
            }
            CommandType::McpPrompt(index) => {
                let prompt = &self.mcp_prompts[*index];
                let source = prompt.completions.then_some(Source::McpPrompt);
                let last = prompt.arguments.len().saturating_sub(1);
                let args: Vec<Arg> = prompt
                    .arguments
                    .iter()
                    .enumerate()
                    .map(|(position, argument)| {
                        let arg = match position == last {
                            true => Arg::text(&argument.name, &argument.description, source),
                            false => Arg::value(&argument.name, &argument.description, source),
                        };
                        match argument.required {
                            true => arg.required(),
                            false => arg,
                        }
                    })
                    .collect();
                read(Node::args(&args))
            }
            CommandType::Lua(index) => {
                let command = &self.lua_commands[*index];
                let source = match command.complete {
                    LuaCompletion::None => None,
                    LuaCompletion::Values(_) | LuaCompletion::Function(()) => Some(Source::Lua),
                    LuaCompletion::Directories => Some(Source::Directories),
                    LuaCompletion::Files => Some(Source::Files),
                };
                let arg = match command.max_args {
                    0 => None,
                    1 => Some(Arg::value(LUA_ARG, "", source)),
                    _ => Some(Arg::text(LUA_ARG, "", source)),
                };
                let arg = arg.map(|arg| match command.required {
                    true => arg.required(),
                    false => arg,
                });
                read(Node::args(arg.as_slice()))
            }
        }
    }

    /// Follows `input` with the caret `caret` chars in. While the caret is
    /// in the command word, it filters commands on what is typed before the
    /// caret, so a command typed in front of a written prompt matches on what
    /// was typed, not on the prompt it touches. Once the word names a command
    /// and the caret is past it, it follows the slot under the caret, and
    /// returns the source the app is asked to [`Self::provide`] for it.
    pub fn sync(&mut self, input: &str, caret: usize, task_focused: bool) -> Option<Source> {
        self.invalidate_mouse_geometry();
        self.refresh_sources();
        self.task_focused = task_focused;
        input.clone_into(&mut self.input);
        if !input.starts_with('/') {
            self.filtered.clear();
            self.current_arg_count = 0;
            self.clear_arguments();
            return None;
        }

        let word_end = input.find(char::is_whitespace).unwrap_or(input.len());
        self.caret = char_to_byte(input, caret);
        self.token_end = match self.caret {
            0 => word_end,
            caret => caret.min(word_end),
        };
        if self.caret > word_end && self.command_named(&input[..word_end]).is_some() {
            self.filtered.clear();
            return self.refresh_arguments();
        }
        self.clear_arguments();
        let cmd_word = &input[1..self.token_end];
        let tail = &input[self.token_end..];
        self.current_arg_count =
            tail.split_whitespace().count() + usize::from(tail.ends_with(char::is_whitespace));

        self.nucleo.pattern.reparse(
            0,
            cmd_word,
            CaseMatching::Ignore,
            Normalization::Smart,
            false,
        );

        self.tick();
        self.hold_abbreviation(cmd_word);
        None
    }

    /// Works out the slot under the caret from [`Self::input`], with what
    /// its source offers so far, and asks an asynchronous source for more.
    fn refresh_arguments(&mut self) -> Option<Source> {
        let word_end = self.input.find(char::is_whitespace)?;
        let command = self.command_named(&self.input[..word_end])?;
        let name = self.item_name(&Match {
            command_type: command.clone(),
            indices: Vec::new(),
        });
        let args = &self.input[word_end..];
        let caret = self.caret - word_end;
        let mut matcher = mem::take(&mut self.matcher);
        let mut read = |values: &[SourceValue]| {
            self.with_node(&command, |node| {
                suggest::arguments(&name, node, args, word_end, caret, values, &mut matcher)
            })
        };
        let Some(mut arguments) = read(&[]) else {
            self.matcher = matcher;
            self.clear_arguments();
            return None;
        };
        let ask = arguments
            .source
            .and_then(|source| self.async_ask(&command, &arguments, source));
        let listed = self.listed_values(&command);
        let values: Option<&[SourceValue]> = match (&ask, arguments.source) {
            (Some(ask), _) => self
                .answered
                .as_ref()
                .filter(|(answered, _)| answered.same_slot(ask))
                .map(|(_, values)| values.as_slice()),
            (None, Some(Source::Lua)) => listed.as_deref(),
            (None, Some(source)) => self
                .supplied
                .as_ref()
                .filter(|(supplied, _)| *supplied == source)
                .map(|(_, values)| values.as_slice()),
            (None, None) => None,
        };
        if let Some(read) = values.and_then(read) {
            arguments = read;
        }
        self.matcher = matcher;
        match ask {
            Some(ask) => self.asking.ask(ask, Instant::now()),
            None => self.asking.clear(),
        }
        if self
            .arguments
            .as_ref()
            .is_none_or(|previous| previous.token != arguments.token)
        {
            self.selected = 0;
            self.scroll_offset = 0;
        }
        self.selected = self
            .selected
            .min(arguments.suggestions.len().saturating_sub(1));
        let wanted = arguments.source.filter(|source| !source.is_async());
        self.arguments = Some(arguments);
        wanted
    }

    fn async_ask(
        &self,
        command: &CommandType,
        arguments: &Arguments,
        source: Source,
    ) -> Option<PaletteAsk> {
        let token = self.input[arguments.token.clone()].to_owned();
        match (source, command) {
            (Source::McpPrompt, CommandType::McpPrompt(index)) => {
                Some(PaletteAsk::Prompt(CompletionAsk {
                    target: CompletionTarget::Prompt {
                        qualified_name: self.mcp_prompts[*index].qualified_name.clone(),
                    },
                    argument: arguments.argument.clone()?,
                    value: token,
                    context: arguments.given.iter().cloned().collect(),
                }))
            }
            (Source::Lua, CommandType::Lua(index)) => {
                let command = &self.lua_commands[*index];
                if command.complete != LuaCompletion::Function(()) {
                    return None;
                }
                let lead = arguments.token.start.min(self.caret)..self.caret;
                Some(PaletteAsk::Lua(CompletionRequest {
                    plugin: Arc::clone(&command.plugin),
                    command: Arc::clone(&command.name),
                    arglead: self.input[lead].to_owned(),
                    cmdline: self.input.clone(),
                    cursorpos: self.caret,
                }))
            }
            (Source::Directories | Source::Files, _) => Some(PaletteAsk::Paths {
                base: self.working_directory.clone()?,
                home: self.home.clone(),
                token,
                directories_only: source == Source::Directories,
            }),
            _ => None,
        }
    }

    /// The fixed list a plugin command offers, which needs no asking.
    fn listed_values(&self, command: &CommandType) -> Option<Vec<SourceValue>> {
        let CommandType::Lua(index) = command else {
            return None;
        };
        let LuaCompletion::Values(values) = &self.lua_commands[*index].complete else {
            return None;
        };
        Some(
            values
                .iter()
                .map(|value| SourceValue::new(value, ""))
                .collect(),
        )
    }

    fn clear_arguments(&mut self) {
        self.arguments = None;
        self.supplied = None;
        self.answered = None;
        self.asking.clear();
    }

    fn hold_abbreviation(&mut self, typed: &str) {
        let Some(&(_, owner)) = HELD_ABBREVIATIONS
            .iter()
            .find(|(abbreviation, _)| abbreviation.eq_ignore_ascii_case(typed))
        else {
            return;
        };
        let owner = CommandRowKey::Builtin(owner);
        if let Some(position) = self
            .filtered
            .iter()
            .position(|m| m.command_type.row_key() == owner)
        {
            self.filtered[..=position].rotate_right(1);
        }
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

    /// Subcommand rows stay out of the bare `/` list, which would otherwise
    /// bury the commands under their verbs, and join as soon as a letter is
    /// typed, behind any command that matches as well.
    fn refresh_matches(&mut self) {
        self.invalidate_mouse_geometry();
        let snapshot = self.nucleo.snapshot();
        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();

        let count = snapshot.matched_item_count();
        let mut ranked: Vec<(u32, Match)> = Vec::with_capacity(count as usize);
        for item in snapshot.matched_items(0..count) {
            let cmd_item = &item.data;
            let col = &item.matcher_columns[0];

            if self.current_arg_count > cmd_item.max_args
                || (!has_pattern && matches!(cmd_item.command_type, CommandType::Subcommand(..)))
            {
                continue;
            }

            let mut indices = Vec::new();
            let score = match has_pattern {
                true => pattern
                    .column_pattern(0)
                    .indices(col.slice(..), &mut self.matcher, &mut indices)
                    .unwrap_or_default(),
                false => 0,
            };
            indices.sort_unstable();
            indices.dedup();
            ranked.push((
                score,
                Match {
                    command_type: cmd_item.command_type.clone(),
                    indices,
                },
            ));
        }
        ranked.sort_by_key(|(score, m)| {
            (
                Reverse(*score),
                matches!(m.command_type, CommandType::Subcommand(..)),
            )
        });
        self.filtered.clear();
        self.filtered.extend(ranked.into_iter().map(|(_, m)| m));

        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.ensure_visible();
    }

    pub fn close(&mut self) {
        self.filtered.clear();
        self.current_arg_count = 0;
        self.scroll_offset = 0;
        self.clear_arguments();
        self.invalidate_mouse_geometry();
    }

    pub fn move_up(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        self.selected = (self.selected + rows - 1) % rows;
        self.ensure_visible();
    }

    pub fn move_down(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        self.selected = (self.selected + 1) % rows;
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
        let max_offset = self.row_count().saturating_sub(self.viewport_height);
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
            CommandType::Subcommand(cmd, sub) => {
                format!("{}{WORD_SEPARATOR}{}", cmd.name, sub.name)
            }
            CommandType::Custom(i) => self.custom[*i].display_name(),
            CommandType::McpPrompt(i) => format!("/{}", self.mcp_prompts[*i].display_name),
            CommandType::Lua(i) => self.lua_commands[*i].name.to_string(),
        }
    }

    fn item_has_args(&self, m: &Match) -> bool {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.max_args() > 0,
            CommandType::Subcommand(_, sub) => sub.continues(),
            CommandType::Custom(i) => self.custom[*i].has_args(),
            CommandType::McpPrompt(i) => !self.mcp_prompts[*i].arguments.is_empty(),
            CommandType::Lua(i) => self.lua_commands[*i].max_args > 0,
        }
    }

    fn item_description(&self, m: &Match) -> &str {
        self.describe(&m.command_type)
    }

    fn is_disabled(&self, m: &Match) -> bool {
        self.task_focused && Self::scope_of(&m.command_type) == ChatScope::MainOnly
    }

    #[cfg(test)]
    pub fn sync_typed(&mut self, input: &str, task_focused: bool) -> Option<Source> {
        self.sync(input, input.chars().count(), task_focused)
    }

    /// What Enter would run on the synced text, which holds no paste chips.
    #[cfg(test)]
    pub fn confirm(&self) -> Option<ParsedCommand> {
        let CommandAction::Run(completion) = self.enter() else {
            return None;
        };
        Some(self.parse(&completion.applied_to(&self.input)))
    }

    /// The command `line` runs, its name spelled as registered, since the
    /// dispatcher matches names exactly and typing ignores case.
    pub fn parse(&self, line: &str) -> ParsedCommand {
        let mut command = ParsedCommand::from_line(line);
        if let Some(name) = self.resolve(&command.name) {
            command.name = name;
        }
        command
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
        Self::items(
            self.features,
            &self.custom,
            &self.mcp_prompts,
            &self.lua_commands,
        )
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

    /// A row's name, a subcommand's parent drawn dimmed, matched chars lit.
    fn name_spans(&self, m: &Match, base: Style, matched: Style) -> Vec<Span<'static>> {
        let name = self.item_name(m);
        let CommandType::Subcommand(parent, _) = m.command_type else {
            return match_spans(&name, &m.indices, base, matched, None);
        };
        let split = parent.name.len() + WORD_SEPARATOR.len();
        let (head, rest) = name.split_at(split);
        let (head_indices, rest_indices): (Vec<u32>, Vec<u32>) = m
            .indices
            .iter()
            .partition(|&&index| (index as usize) < split);
        let rest_indices: Vec<u32> = rest_indices
            .into_iter()
            .map(|index| index - split as u32)
            .collect();
        let mut spans = match_spans(
            head,
            &head_indices,
            theme::dim_style(base, DISABLED_DIM),
            matched,
            None,
        );
        spans.extend(match_spans(rest, &rest_indices, base, matched, None));
        spans
    }

    pub fn view(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        if self.arguments.is_some() {
            return self.view_arguments(frame, input_area);
        }
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

        let names: Vec<String> = filtered.iter().map(|m| self.item_name(m)).collect();
        let signatures: Vec<String> = filtered
            .iter()
            .map(|m| truncate(&self.signature_of(&m.command_type), SIGNATURE_WIDTH))
            .collect();
        let max_name = names
            .iter()
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);
        let max_signature = signatures
            .iter()
            .map(|signature| signature.chars().count())
            .max()
            .unwrap_or(0);
        let signature_column = match max_signature {
            0 => 0,
            width => width + GAP,
        };
        let max_desc = filtered
            .iter()
            .map(|item| self.item_description(item).chars().count())
            .max()
            .unwrap_or(0);
        let popup_width = (PAD + max_name + GAP + signature_column + max_desc + PAD) as u16;

        let popup = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width.min(input_area.width),
            height: popup_height,
        };
        grab_scope!("command", popup);

        let t = theme::current();
        let viewport_height = popup_height as usize;
        let scroll_offset = self
            .scroll_offset
            .min(filtered.len().saturating_sub(viewport_height));
        let end = (scroll_offset + viewport_height).min(filtered.len());
        let lines: Vec<Line> = (scroll_offset..end)
            .map(|i| {
                let m = &filtered[i];
                let selected = i == self.selected;
                let dim = |style: Style| match self.is_disabled(m) {
                    true => theme::dim_style(style, DISABLED_DIM),
                    false => style,
                };
                let (base, matched, detail) = match selected {
                    true => (t.item_selected, t.item_match_selected, t.item_selected),
                    false => (t.item, t.item_match, t.item_desc),
                };
                let fill = |width: usize| Span::styled(" ".repeat(width), base);
                let name_pad = max_name - names[i].chars().count() + GAP;
                let mut spans = vec![fill(PAD)];
                spans.extend(self.name_spans(m, dim(base), dim(matched)));
                spans.push(fill(name_pad));
                if signature_column > 0 {
                    let signature = &signatures[i];
                    spans.push(Span::styled(
                        signature.clone(),
                        theme::dim_style(dim(detail), DISABLED_DIM),
                    ));
                    spans.push(fill(signature_column - signature.chars().count()));
                }
                spans.push(Span::styled(
                    self.item_description(m).to_owned(),
                    dim(detail),
                ));
                spans.push(fill(PAD));
                Line::from(spans)
            })
            .collect();

        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(t.surface_style()), popup);

        self.row_hits = (scroll_offset..end)
            .enumerate()
            .map(|(row, index)| CommandRowHit {
                area: Rect::new(popup.x, popup.y + row as u16, popup.width, 1),
                row_index: index,
                key: filtered[index].command_type.row_key(),
            })
            .collect();
        self.popup_area = Some(popup);
        self.viewport_height = viewport_height;
        self.scroll_offset = scroll_offset;

        Some(popup)
    }

    /// The argument popup, standing on the composer.
    fn view_arguments(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        self.invalidate_mouse_geometry();
        let width = arguments_width(self.arguments.as_ref()?);
        let height = self.argument_rows().min(input_area.y);
        if height == 0 {
            return None;
        }
        let popup = Rect {
            x: input_area.x,
            y: input_area.y - height,
            width: width.min(input_area.width),
            height,
        };
        self.draw_arguments(frame, popup);
        Some(popup)
    }

    /// Rows the argument list asks for: the signature, every suggestion,
    /// and the placeholder. Zero while no command's arguments are followed.
    pub fn argument_rows(&self) -> u16 {
        self.arguments.as_ref().map_or(0, |arguments| {
            let placeholder = usize::from(arguments.placeholder.is_some());
            (1 + arguments.suggestions.len() + placeholder) as u16
        })
    }

    /// The argument list over `area`, for a surface that places it itself.
    pub fn view_arguments_in(&mut self, frame: &mut Frame, area: Rect) {
        self.invalidate_mouse_geometry();
        if area.height > 0 && self.arguments.is_some() {
            self.draw_arguments(frame, area);
        }
    }

    /// The signature with the caret's slot lit, the suggestions for that
    /// slot, then the open slot's placeholder, which the selection never
    /// lands on.
    fn draw_arguments(&mut self, frame: &mut Frame, popup: Rect) {
        let Some(arguments) = self.arguments.as_ref() else {
            return;
        };
        grab_scope!("command", popup);
        let suggestions = &arguments.suggestions;
        let placeholder = usize::from(arguments.placeholder.is_some());
        let height = popup.height as usize;
        let viewport = height
            .saturating_sub(1 + placeholder)
            .min(suggestions.len());
        let shows_placeholder = height > 1 + viewport;
        let scroll_offset = self
            .scroll_offset
            .min(suggestions.len().saturating_sub(viewport));
        let max_value = value_width(arguments);

        let t = theme::current();
        let mut signature = vec![Span::raw(" ".repeat(PAD))];
        for (index, part) in arguments.signature.iter().enumerate() {
            if index > 0 {
                signature.push(Span::raw(WORD_SEPARATOR));
            }
            let style = match part.current {
                true => t.item.add_modifier(Modifier::BOLD),
                false => t.item_desc,
            };
            signature.push(Span::styled(part.text.clone(), style));
        }
        let mut lines = vec![Line::from(signature)];
        let end = scroll_offset + viewport;
        for (index, suggestion) in suggestions[scroll_offset..end].iter().enumerate() {
            let index = scroll_offset + index;
            let (base, matched, detail) = match index == self.selected {
                true => (t.item_selected, t.item_match_selected, t.item_selected),
                false => (t.item, t.item_match, t.item_desc),
            };
            let fill = |width: usize| Span::styled(" ".repeat(width), base);
            let mut spans = vec![fill(PAD)];
            spans.extend(match_spans(
                &suggestion.value,
                &suggestion.indices,
                base,
                matched,
                None,
            ));
            spans.push(fill(max_value - suggestion.value.chars().count() + GAP));
            spans.push(Span::styled(suggestion.description.clone(), detail));
            spans.push(fill(PAD));
            lines.push(Line::from(spans));
        }
        if let Some(placeholder) = arguments.placeholder.as_ref().filter(|_| shows_placeholder) {
            let label_pad = max_value - placeholder.label.chars().count() + GAP;
            lines.push(Line::from(Span::styled(
                format!(
                    "{}{}{}{}",
                    " ".repeat(PAD),
                    placeholder.label,
                    " ".repeat(label_pad),
                    placeholder.description
                ),
                t.input_placeholder,
            )));
        }

        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(t.surface_style()), popup);

        self.row_hits = (scroll_offset..end)
            .enumerate()
            .map(|(row, index)| CommandRowHit {
                area: Rect::new(popup.x, popup.y + 1 + row as u16, popup.width, 1),
                row_index: index,
                key: CommandRowKey::Suggestion(index),
            })
            .collect();
        self.popup_area = Some(popup);
        self.viewport_height = viewport;
        self.scroll_offset = scroll_offset;
    }
}

/// The widest value or placeholder label in the list.
fn value_width(arguments: &Arguments) -> usize {
    let values = arguments
        .suggestions
        .iter()
        .map(|suggestion| &suggestion.value);
    let label = arguments
        .placeholder
        .iter()
        .map(|placeholder| &placeholder.label);
    values
        .chain(label)
        .map(|text| text.chars().count())
        .max()
        .unwrap_or(0)
}

/// Columns the argument list needs, so neither a row nor the signature is cut.
fn arguments_width(arguments: &Arguments) -> u16 {
    let signature = arguments
        .signature
        .iter()
        .map(|part| part.text.chars().count() + WORD_SEPARATOR.len())
        .sum::<usize>();
    let descriptions = arguments
        .suggestions
        .iter()
        .map(|suggestion| &suggestion.description);
    let placeholder = arguments
        .placeholder
        .iter()
        .map(|placeholder| &placeholder.description);
    let description = descriptions
        .chain(placeholder)
        .map(|text| text.chars().count())
        .max()
        .unwrap_or(0);
    let rows = PAD + value_width(arguments) + GAP + description + PAD;
    rows.max(PAD + signature + PAD) as u16
}

/// What `value` adds past `typed`, when `typed` is how it starts.
fn rest_of(value: &str, typed: &str) -> Option<String> {
    let head = value.get(..typed.len())?;
    (head.eq_ignore_ascii_case(typed) && value.len() > typed.len())
        .then(|| value[typed.len()..].to_owned())
}

/// An argument hint such as `<file>` as a slot name. One with several parts
/// stays as written.
fn hint_name(hint: &str) -> &str {
    let hint = hint.trim();
    let inner = hint.trim_matches(HINT_BRACKETS);
    match inner.contains(HINT_BRACKETS) {
        true => hint,
        false => inner,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::components::mcp_completion::COMPLETION_DEBOUNCE;
    use caudra_agent::mcp::completing_stub_session;
    use caudra_agent::mcp::resources::ResourceLists;
    use caudra_agent::{McpPromptArg, McpSnapshot};
    use caudra_config::Feature;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use test_case::test_case;

    fn empty_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::empty()
    }

    fn synced(input: &str) -> CommandPalette {
        synced_with(input, FeatureFlags::all())
    }

    fn synced_with(input: &str, features: FeatureFlags) -> CommandPalette {
        let mut p = CommandPalette::new(
            Arc::from([]),
            empty_snapshot(),
            LuaCommandReader::empty(),
            features,
        );
        p.sync_typed(input, false);
        p
    }

    #[test_case("/workflow", Feature::Workflows; "workflow")]
    #[test_case("/peers", Feature::CrossSessionMessaging; "peers")]
    #[test_case("/messages", Feature::CrossSessionMessaging; "messages")]
    #[test_case("/topics", Feature::CrossSessionMessaging; "topics")]
    #[test_case("/groups", Feature::CrossSessionMessaging; "groups")]
    #[test_case("/deep-research", Feature::Workflows; "workflow_shortcut")]
    #[test_case("/automations", Feature::Automations; "automations")]
    #[test_case("/sandbox", Feature::Sandboxes; "sandbox")]
    fn a_command_is_absent_until_its_experiment_is_on(name: &str, feature: Feature) {
        let mut off = synced_with(name, FeatureFlags::all().without(feature));
        assert!(off.rows(false).iter().all(|row| row.name != name));
        assert_eq!(off.resolve(name), None);
        assert!(off.confirm().is_none_or(|command| command.name != name));
        assert_eq!(
            disabled_feature(name, FeatureFlags::all().without(feature)),
            Some(feature)
        );

        let mut on = synced_with(name, FeatureFlags::NONE.with(feature));
        assert!(on.rows(false).iter().any(|row| row.name == name));
        assert_eq!(on.resolve(name).as_deref(), Some(name));
        assert_eq!(
            disabled_feature(name, FeatureFlags::NONE.with(feature)),
            None
        );
    }

    #[test_case(FeatureFlags::NONE => false; "neither")]
    #[test_case(FeatureFlags::NONE.with(Feature::Sandboxes) => true; "managed_sandboxes")]
    #[test_case(FeatureFlags::NONE.with(Feature::RemoteWorkcell) => true; "direct_remote")]
    fn remote_follows_either_remote_experiment(features: FeatureFlags) -> bool {
        const REMOTE: &str = "/remote";
        synced_with(REMOTE, features).resolve(REMOTE).is_some()
    }

    #[test_case("/memory"; "memory")]
    #[test_case("/learn"; "learn")]
    #[test_case("/auto"; "auto")]
    #[test_case("/decisions"; "decisions")]
    fn ordinary_commands_need_no_experiment(name: &str) {
        assert_eq!(disabled_feature(name, FeatureFlags::NONE), None);
        assert_eq!(
            synced_with(name, FeatureFlags::NONE)
                .resolve(name)
                .as_deref(),
            Some(name)
        );
    }

    #[test_case(words::MOVE_ARG; "current")]
    #[test_case(words::MIGRATE_ARG; "bulk")]
    fn relocation_subcommands_are_main_only_and_preserve_directory_spaces(subcommand: &str) {
        const SESSIONS: &str = "/sessions";
        const DIRECTORY: &str = "../project with spaces";
        let args = format!("{subcommand} {DIRECTORY}");
        let mut palette = synced(&format!("{SESSIONS} {args}"));
        let command = palette.confirm().unwrap();
        assert_eq!(
            (command.name.as_str(), command.args.as_str()),
            (SESSIONS, args.as_str())
        );
        assert!(palette.is_main_only(SESSIONS, &args));
        assert!(!palette.is_main_only(SESSIONS, ""));
        let row_name = format!("{SESSIONS} {subcommand}");
        let row = palette
            .rows(true)
            .into_iter()
            .find(|row| row.name == row_name)
            .unwrap();
        assert!(row.disabled);
        assert!(row.takes_args());
    }

    #[test]
    fn fork_is_main_only_and_takes_no_arguments() {
        const COMMAND: &str = "/fork";
        let mut palette = synced(COMMAND);
        assert_eq!(palette.confirm().unwrap().name, COMMAND);
        assert!(palette.is_main_only(COMMAND, ""));
        let row = palette
            .rows(true)
            .into_iter()
            .find(|row| row.name == COMMAND)
            .unwrap();
        assert!(row.disabled);
        assert!(!row.takes_args());
    }

    #[test_case(false; "main")]
    #[test_case(true; "task")]
    fn decisions_diagnostics_are_available_without_arguments(task_focused: bool) {
        const COMMAND: &str = "/decisions";
        let mut palette = synced(COMMAND);
        let row = palette
            .rows(task_focused)
            .into_iter()
            .find(|row| row.name == COMMAND)
            .unwrap();
        assert!(!row.disabled);
        assert_eq!(palette.confirm().unwrap().name, COMMAND);
        assert!(!palette.is_main_only(COMMAND, ""));
        assert!(palette.is_main_only(COMMAND, words::MODEL_ARG));
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

    /// The foreground the popup actually painted the command's name in, which
    /// is the only place the dimming decision becomes observable.
    fn row_fg(palette: &mut CommandPalette, command: &str) -> ratatui::style::Color {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                palette.view(frame, Rect::new(0, 20, 80, 4));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let line: String = (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect();
            if let Some(column) = line.find(command) {
                return buffer.cell((column as u16, y)).unwrap().fg;
            }
        }
        panic!("{command} was not rendered");
    }

    fn palette_for_scope(task_focused: bool) -> CommandPalette {
        let mut palette = CommandPalette::new(
            Arc::from([]),
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        palette.sync_typed("/", task_focused);
        // Off the first row, so both commands under test are styled as
        // ordinary rows rather than as the selection.
        palette.move_down();
        palette
    }

    #[test_case("/compact", true ; "main_only_dims")]
    #[test_case("/help", false ; "anywhere_keeps_its_color")]
    fn task_focus_dims_only_main_only_rows(command: &str, expected_dimmed: bool) {
        let main = row_fg(&mut palette_for_scope(false), command);
        let task = row_fg(&mut palette_for_scope(true), command);

        assert_eq!(main != task, expected_dimmed);
    }

    #[test_case("/btw" ; "btw")]
    #[test_case("/catchup" ; "catchup")]
    fn side_questions_run_in_a_task_chat(command: &str) {
        assert!(!palette_for_scope(true).is_main_only(command, ""));
    }

    fn synced_with_custom(input: &str, custom: Arc<[CustomCommand]>) -> CommandPalette {
        let mut p = CommandPalette::new(
            custom,
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        p.sync_typed(input, false);
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
                argument_hint: None,
                source: caudra_agent::command::CommandSource::Local(PathBuf::from(
                    ".caudra/commands/review.md",
                )),
            },
            CustomCommand {
                name: "fix".into(),
                description: "Quick fix".into(),
                content: "Fix the code".into(),
                scope: caudra_agent::command::CommandScope::User,
                accepts_args: false,
                argument_hint: None,
                source: caudra_agent::command::CommandSource::Local(PathBuf::from(
                    "/config/caudra/commands/fix.md",
                )),
            },
        ])
    }

    #[test_case("/tasks", true; "plural")]
    #[test_case("/task", false; "no_singular_alias")]
    fn task_command_requires_plural(name: &str, recognized: bool) {
        assert_eq!(synced("/").resolve(name).is_some(), recognized);
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

    #[test_case("/goal-c", "/goal clear" ; "clear")]
    #[test_case("/goal-m", "/goal model" ; "model")]
    #[test_case("/pop", "/stash pop" ; "a_verb_alone")]
    #[test_case("/stash-l", "/stash list" ; "an_old_dashed_spelling")]
    fn subcommands_are_found_by_their_verb(input: &str, expected: &str) {
        let p = synced(input);
        assert_eq!(p.item_name(&p.filtered[0]), expected);
    }

    #[test]
    fn a_bare_slash_hides_subcommands() {
        let p = synced("/");
        assert!(
            !p.filtered
                .iter()
                .any(|item| matches!(item.command_type, CommandType::Subcommand(..)))
        );
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
            palette.handle_mouse(mouse(MouseEventKind::Moved, hit.area)),
            CommandAction::Consumed
        ));
        assert_eq!(palette.selected, hit.row_index);
    }

    #[test]
    fn clicking_command_executes_with_enter_semantics() {
        let mut palette = synced("/");
        render(&mut palette);
        let hit = palette.row_hits[1];
        let expected_name = palette.item_name(&palette.filtered[hit.row_index]);

        palette.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        let action = palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(
            action,
            CommandAction::Run(Completion { text, .. }) if text == expected_name
        ));
        assert!(!palette.is_active());
    }

    #[test]
    fn releasing_on_another_command_does_not_execute() {
        let mut palette = synced("/");
        render(&mut palette);
        let first = palette.row_hits[0];
        let second = palette.row_hits[1];

        palette.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first.area));
        let action =
            palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.area));

        assert!(matches!(action, CommandAction::Consumed));
        assert!(palette.is_active());
    }

    #[test]
    fn dragging_command_cancels_click() {
        let mut palette = synced("/");
        render(&mut palette);
        let hit = palette.row_hits[0];

        palette.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        palette.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), hit.area));
        let action = palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(action, CommandAction::Consumed));
        assert!(palette.is_active());
    }

    #[test]
    fn command_filtering_invalidates_rendered_popup() {
        let mut palette = synced("/");
        render(&mut palette);
        let stale = palette.row_hits[0];
        palette.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));

        palette.sync_typed("/goal", false);
        let action = palette.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

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
            palette.handle_mouse(mouse(MouseEventKind::Moved, outside)),
            CommandAction::Passthrough
        ));
    }

    #[test]
    fn confirm_when_inactive_returns_none() {
        let p = CommandPalette::new(
            Arc::from([]),
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        assert!(p.confirm().is_none());
    }

    #[test]
    fn sync_clamps_selected() {
        let mut p = synced("/");
        p.selected = 100;
        p.sync_typed("/", false);
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn a_named_command_moves_on_to_its_arguments() {
        let p = synced("/cd ~/foo");
        assert!(p.is_active());
        assert!(p.filtered.is_empty());
        assert!(p.arguments.is_some());
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
    #[test_case("/cpt", "/compact", ""    ; "fuzzy-match-2")]
    #[test_case("/p", "/permissions", ""  ; "p_keeps_opening_permissions")]
    #[test_case("/f", "/fast", ""  ; "f_keeps_opening_fast")]
    #[test_case("/fo", "/fork", ""  ; "fo_opens_fork")]
    #[test_case("/pr", "/projection", ""  ; "pr_opens_projection")]
    #[test_case("/proj", "/projection", "" ; "proj_opens_projection")]
    #[test_case("/btw hello world", "/btw", "hello world" ; "btw_multi_word")]
    fn confirm_parses_args(input: &str, expected_name: &str, expected_args: &str) {
        let mut p = CommandPalette::new(
            Arc::from([]),
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        p.sync_typed(input, false);
        let cmd = p.confirm().unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.args, expected_args);
    }

    #[test]
    fn confirm_custom_command() {
        let custom = sample_custom();
        let mut p = CommandPalette::new(
            custom,
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        p.sync_typed("/project:review some-file.rs", false);
        assert!(p.is_active());
        let cmd = p.confirm().unwrap();
        assert_eq!(cmd.name, "/project:review");
        assert_eq!(cmd.args, "some-file.rs");
    }

    #[test]
    fn find_custom_command_lookup() {
        let custom = sample_custom();
        let p = CommandPalette::new(
            custom,
            empty_snapshot(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
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
                    completions: false,
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
                    completions: false,
                    arguments: vec![],
                },
            ],
            resources: Vec::new(),
            pids: vec![],
            generation: 0,
        })
    }

    fn synced_with_prompts(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(
            Arc::from([]),
            sample_prompts(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        p.sync_typed(input, false);
        p
    }

    #[test]
    fn filter_mcp_prompt_by_substring() {
        let p = synced_with_prompts("/code-r");
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
        let p = synced_with_prompts("/myserver:code-review my-diff-content");
        let cmd = p.confirm().unwrap();
        assert_eq!(cmd.name, "/myserver:code-review");
        assert_eq!(cmd.args, "my-diff-content");
    }

    #[test]
    fn mcp_update_clears_old_prompts() {
        let reader = sample_prompts();
        let mut p = CommandPalette::new(
            Arc::from([]),
            reader,
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );

        p.sync_typed("/", false);
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
                completions: false,
                arguments: vec![],
            }],
            resources: Vec::new(),
            pids: vec![],
            generation: 1,
        });

        p.mcp_reader = updated_reader;
        p.sync_typed("/", false);

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

    const ADOPTED_PROMPTS_MSG: &str =
        "a restarted manager's prompts must load even when its generation matches the old one";

    #[test]
    fn adopting_a_reader_at_the_same_generation_loads_its_prompts() {
        let mut p = CommandPalette::new(
            Arc::from([]),
            McpSnapshotReader::empty(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        );
        p.sync_typed("/", false);

        p.set_mcp_reader(sample_prompts());
        p.sync_typed("/", false);

        let prompts = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .count();
        assert_eq!(
            prompts,
            sample_prompts().load().prompts.len(),
            "{ADOPTED_PROMPTS_MSG}"
        );
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
                ..LuaCommandInfo::default()
            },
            LuaCommandInfo {
                name: Arc::from("/deploy"),
                description: Arc::from("Deploy the project"),
                plugin: Arc::from("deploy_plugin"),
                max_args: 0,
                ..LuaCommandInfo::default()
            },
        ])
    }

    fn synced_with_nargs(input: &str, max_args: usize) -> CommandPalette {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from("/deploy"),
            description: Arc::from("Deploy the project"),
            plugin: Arc::from("deploy_plugin"),
            max_args,
            ..LuaCommandInfo::default()
        }]);
        let mut p =
            CommandPalette::new(Arc::from([]), empty_snapshot(), reader, FeatureFlags::all());
        p.sync_typed(input, false);
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
        let cmd = synced_with_nargs(input, usize::MAX).confirm().unwrap();
        assert_eq!(cmd.name, "/deploy");
        assert_eq!(cmd.args, "my new target");
    }

    fn synced_with_lua(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(
            Arc::from([]),
            empty_snapshot(),
            sample_lua_commands(),
            FeatureFlags::all(),
        );
        p.sync_typed(input, false);
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
        let cmd = synced_with_nargs("/deploy some-arg", 1).confirm().unwrap();
        assert_eq!(cmd.name, "/deploy");
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
            ..LuaCommandInfo::default()
        }]);
        let mut p =
            CommandPalette::new(Arc::from([]), empty_snapshot(), reader, FeatureFlags::all());
        p.sync_typed("/", false);
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
                ..LuaCommandInfo::default()
            },
            LuaCommandInfo {
                name: Arc::from("/new2"),
                description: Arc::from("new2"),
                plugin: Arc::from("p"),
                max_args: 0,
                ..LuaCommandInfo::default()
            },
        ]);
        p.sync_typed("/", false);
        let updated_lua = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(updated_lua, 2);
        assert!(p.find_lua_command("/old").is_none());
        assert!(p.find_lua_command("/new1").is_some());
    }

    const LUA_PICK: &str = "/pick";
    const LUA_PICK_PLUGIN: &str = "picker";
    const LUA_LISTED: [&str; 2] = ["alpha", "beta"];
    const LUA_REQUIRED_SIGNATURE: &str = "<args>";
    const NOT_ASKED: &str = "the plugin's complete function was never asked";

    fn lua_listed() -> Vec<String> {
        LUA_LISTED.map(String::from).to_vec()
    }

    fn lua_palette(complete: LuaCompletion, handle: EventHandle) -> CommandPalette {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from(LUA_PICK),
            plugin: Arc::from(LUA_PICK_PLUGIN),
            max_args: 1,
            required: true,
            complete,
            ..LuaCommandInfo::default()
        }]);
        CommandPalette::new(Arc::from([]), empty_snapshot(), reader, FeatureFlags::all())
            .with_lua_handle(handle)
    }

    #[test_case("/pick ", &LUA_LISTED ; "every_value_before_a_word")]
    #[test_case("/pick al", &LUA_LISTED[..1] ; "the_values_the_word_matches")]
    fn a_plugin_list_is_suggested(input: &str, expected: &[&str]) {
        let mut palette = lua_palette(
            LuaCompletion::Values(lua_listed().into()),
            EventHandle::inert(),
        );
        palette.sync_typed(input, false);
        assert_eq!(suggested(&palette), expected);
    }

    #[test]
    fn a_plugin_function_is_asked_with_nvims_arguments() {
        let (handle, probe) = caudra_lua::test_support::probed_event_handle();
        let mut palette = lua_palette(LuaCompletion::Function(()), handle);
        let input = "/pick al";
        palette.sync_typed(input, false);
        let answering = std::thread::spawn(move || probe.answer_completion(lua_listed()));

        answered(&mut palette);

        assert_eq!(suggested(&palette), &LUA_LISTED[..1]);
        drop(palette);
        let asked = answering.join().unwrap().expect(NOT_ASKED);
        assert_eq!(
            (
                asked.arglead.as_str(),
                asked.cmdline.as_str(),
                asked.cursorpos
            ),
            ("al", input, input.len())
        );
    }

    #[test]
    fn a_required_plugin_argument_is_signed_as_one() {
        let mut palette = lua_palette(LuaCompletion::None, EventHandle::inert());
        let row = palette
            .rows(false)
            .into_iter()
            .find(|row| row.name == LUA_PICK)
            .unwrap();
        assert_eq!(row.signature, LUA_REQUIRED_SIGNATURE);
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
        palette.row_hits.iter().map(|hit| hit.row_index).collect()
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
        assert_eq!(hit.row_index, palette.scroll_offset);
        assert_eq!(
            hit.key,
            palette.filtered[hit.row_index].command_type.row_key()
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

    const COMPLETING_PROMPT: &str = "review";
    const BRANCH: &str = "branch";
    const PATH: &str = "path";
    const SUGGESTED: [&str; 2] = ["main", "maint"];
    const NOT_TRACKED: &str = "the palette lost the argument being typed";

    fn completing_palette() -> CommandPalette {
        let session = completing_stub_session(
            ResourceLists::default(),
            COMPLETING_PROMPT,
            &[BRANCH, PATH],
            &SUGGESTED,
        );
        CommandPalette::new(
            Arc::from([]),
            session.reader(),
            LuaCommandReader::empty(),
            FeatureFlags::all(),
        )
    }

    /// Waits for the server's answer, past the pause it waits out.
    fn answered(palette: &mut CommandPalette) {
        let due = Instant::now() + COMPLETION_DEBOUNCE;
        let reader = palette.mcp_reader.clone();
        let lua = palette.lua_handle.clone();
        if let Some(answer) = palette.asking.settle(due, |ask| ask.start(&reader, &lua)) {
            palette.answered = Some(answer);
            palette.refresh_arguments();
        }
    }

    fn suggested(palette: &CommandPalette) -> Vec<&str> {
        palette
            .arguments
            .as_ref()
            .expect(NOT_TRACKED)
            .suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect()
    }

    #[test_case("/stub:review ma", BRANCH, "ma", &[] ; "the_first_argument")]
    #[test_case("/stub:review ", BRANCH, "", &[] ; "an_argument_not_started")]
    #[test_case("/stub:review main pa", PATH, "pa", &[(BRANCH, "main")] ; "a_later_argument_carries_the_earlier")]
    #[test_case("/stub:review main docs/my file", PATH, "docs/my file", &[(BRANCH, "main")] ; "the_last_argument_takes_the_rest")]
    fn the_argument_being_typed_is_asked_with_the_ones_before(
        input: &str,
        argument: &str,
        value: &str,
        context: &[(&str, &str)],
    ) {
        let mut palette = completing_palette();
        palette.sync_typed(input, false);
        let Some(PaletteAsk::Prompt(asked)) = palette.asking.pending() else {
            panic!("{NOT_TRACKED}");
        };
        let context: BTreeMap<String, String> = context
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        assert_eq!(
            (
                asked.argument.as_str(),
                asked.value.as_str(),
                &asked.context
            ),
            (argument, value, &context)
        );
    }

    #[test]
    fn a_suggested_value_completes_the_argument() {
        let mut palette = completing_palette();
        let input = "/stub:review ma";
        palette.sync_typed(input, false);
        answered(&mut palette);
        assert_eq!(suggested(&palette), SUGGESTED);
        palette.handle_key(KeyEvent::from(KeyCode::Down));
        let CommandAction::Complete(completion) = palette.handle_key(KeyEvent::from(KeyCode::Tab))
        else {
            panic!("Tab did not take the suggestion");
        };
        let text = format!("{} ", SUGGESTED[1]);
        assert_eq!(
            completion,
            Completion {
                range: "/stub:review ".len()..input.len(),
                caret: text.len(),
                text,
            }
        );
    }

    const TAIL_LOST: &str = "Tab must splice the command over the word and keep what follows";

    fn tab(palette: &mut CommandPalette) -> Completion {
        match palette.handle_key(KeyEvent::from(KeyCode::Tab)) {
            CommandAction::Complete(completion) => completion,
            _ => panic!("Tab did not complete"),
        }
    }

    fn spliced(input: &str, completion: &Completion) -> (String, usize) {
        let end = char_to_byte(input, completion.range.end);
        let text = format!("{}{}", completion.text, &input[end..]);
        (text, completion.range.start + completion.caret)
    }

    #[test_case("/fo", 3, "/fork", 5 ; "zero_arg_command_alone")]
    #[test_case("/ren", 4, "/rename ", 8 ; "command_with_arguments_alone")]
    #[test_case("/goFix the bug", 3, "/goal Fix the bug", 6 ; "caret_inside_glued_prose")]
    #[test_case("/go Fix the bug", 3, "/goal Fix the bug", 6 ; "prose_after_a_space")]
    #[test_case("/go Fix the bug", 0, "/goal Fix the bug", 6 ; "caret_at_the_start")]
    #[test_case("/go Fix the bug", 9, "/goal Fix the bug", 6 ; "caret_in_the_prose")]
    fn tab_completes_only_the_command_word(
        input: &str,
        caret: usize,
        expected: &str,
        expected_caret: usize,
    ) {
        let mut palette = synced("/");
        palette.sync(input, caret, false);
        let expected_name = expected.split_whitespace().next().unwrap();
        palette.selected = palette
            .filtered
            .iter()
            .position(|item| palette.item_name(item) == expected_name)
            .unwrap();
        let completion = tab(&mut palette);
        assert_eq!(
            spliced(input, &completion),
            (expected.to_owned(), expected_caret),
            "{TAIL_LOST}"
        );
    }

    #[test]
    fn enter_runs_the_command_with_the_prose_after_it() {
        let mut palette = synced("/");
        palette.sync("/goFix the bug", 3, false);
        palette.selected = palette
            .filtered
            .iter()
            .position(|item| palette.item_name(item) == "/goal")
            .unwrap();
        let command = palette.confirm().unwrap();
        assert_eq!(
            (command.name.as_str(), command.args.as_str()),
            ("/goal", "Fix the bug")
        );
    }

    /// Typing on keeps the suggestions that still fit, while the next answer
    /// is on its way.
    #[test]
    fn typing_on_keeps_only_the_values_that_still_fit() {
        let mut palette = completing_palette();
        palette.sync_typed("/stub:review ma", false);
        answered(&mut palette);
        palette.sync_typed("/stub:review mai", false);
        palette.sync_typed("/stub:review main", false);
        palette.sync_typed("/stub:review maint", false);
        assert_eq!(suggested(&palette), [SUGGESTED[1]]);
    }

    #[test]
    fn a_server_without_completions_is_never_asked() {
        let palette = synced_with_prompts("/myserver:code-review ma");
        assert!(palette.asking.pending().is_none());
    }
}
