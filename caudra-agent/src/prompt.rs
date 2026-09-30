use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use caudra_config::{
    AgentConfig, ExecutionMode, effective_shell_execution, effective_task_execution,
};
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

pub mod profile;

use profile::{PromptProfileLayout, SystemPromptProfile};

const EXECUTION_HINT_OWNER: &str = "native:execution";
const TASK_SYNC_GUIDANCE: &str = "Task calls wait for the completed result. Batch runs independent calls concurrently and returns their results together.";
const TASK_AUTO_GUIDANCE: &str = "Task calls default to foreground execution (background: false), waiting for the completed result. Use background: true for independent work, including inside batch: the call returns an admission receipt and reports and final results arrive automatically. Batch alone does not make foreground tasks asynchronous. task_control can promote an active foreground task without restarting it.";
const TASK_ASYNC_GUIDANCE: &str = "Task calls launch background work and return an admission receipt. Omit background or set it to true. Reports and final results arrive automatically, including after you end your turn.";
const ASYNC_RESULT_GUIDANCE: &str = "An admission receipt is not completion or success. Continue independent work without duplicating pending work or concurrently editing the same files. Do not poll, sleep, or repeat a launch. If only pending work remains, state what is pending and return control without claiming completion. Later results arrive at a safe boundary; evaluate them against current user instructions and verify claims before continuing.";

pub fn task_execution_guidance(mode: &ExecutionMode) -> String {
    let delivery = match mode {
        ExecutionMode::Sync => TASK_SYNC_GUIDANCE,
        ExecutionMode::Auto => TASK_AUTO_GUIDANCE,
        ExecutionMode::Async => TASK_ASYNC_GUIDANCE,
    };
    if *mode == ExecutionMode::Sync {
        delivery.into()
    } else {
        format!("{delivery}\n\n{ASYNC_RESULT_GUIDANCE}")
    }
}

pub fn shell_execution_guidance(mode: &ExecutionMode, threshold_secs: u64) -> String {
    match mode {
        ExecutionMode::Sync => "Shell calls wait for termination and return the terminal result. timeoutSec is the enforced execution deadline.".into(),
        ExecutionMode::Auto => format!("Shell delivery is selected at admission, never promoted by elapsed runtime. By default, an effective timeout at or below {threshold_secs} seconds returns the terminal result synchronously; above {threshold_secs} seconds it returns an async admission receipt and the terminal result arrives automatically. When configured duration estimates are enforced, the lesser of the estimate and execution deadline selects delivery against that threshold. The tool's default applies when timeoutSec is omitted unless an enabled duration estimate supplies a longer bounded default. Explicit timeoutSec is never changed and remains the enforced execution deadline; never extend it just to influence scheduling.\n\n{ASYNC_RESULT_GUIDANCE}"),
        ExecutionMode::Async => format!("Shell calls return an async admission receipt and the terminal result arrives automatically. timeoutSec remains the enforced execution deadline.\n\n{ASYNC_RESULT_GUIDANCE}"),
    }
}

pub fn execution_guidance(
    config: &AgentConfig,
    task_background_supported: bool,
    shell_background_supported: bool,
    task_exposed: bool,
    shell_exposed: bool,
) -> String {
    let mut fragments = Vec::new();
    if task_exposed && let Some(mode) = effective_task_execution(config, task_background_supported)
    {
        fragments.push(task_execution_guidance(&mode));
    }
    if shell_exposed
        && let Some(mode) = effective_shell_execution(config, shell_background_supported)
    {
        fragments.push(shell_execution_guidance(
            &mode,
            config.shell_async_threshold_secs,
        ));
    }
    fragments.join("\n\n")
}

pub trait ValidNames: IntoEnumIterator + std::fmt::Display {
    fn valid_names() -> String {
        Self::iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
const SYSTEM_STYLE: &str = include_str!("prompts/system_style.md");
const SYSTEM_TOOLS: &str = include_str!("prompts/system_tools.md");
const SYSTEM_CONVENTIONS: &str = include_str!("prompts/system_conventions.md");
const SYSTEM_COMPLETION: &str = include_str!("prompts/system_completion.md");
/// Announced in the conversation rather than the system prompt: the system
/// block sits ahead of every message in the cache prefix, so varying it by mode
/// re-caches the whole history on each toggle.
pub const PLAN_PROMPT: &str = include_str!("prompts/plan.md");
pub const BUILD_PROMPT: &str = include_str!("prompts/build.md");
/// The mode-invariant half, which stays in the system prompt.
pub const MODES_PROMPT: &str = include_str!("prompts/modes.md");
/// Standing reminders reach the model as user-role observations, so the tag is
/// all that separates them from something the user typed.
pub const REMINDERS_PROMPT: &str = include_str!("prompts/reminders.md");
/// Both standing sections, in the order they are read: the contract that
/// governs every reminder, then the mode rules that are its highest-stakes
/// instance.
pub const STANDING_PROMPT: &str = concat!(
    include_str!("prompts/reminders.md"),
    include_str!("prompts/modes.md"),
);
/// Announced for the same reason: the date rolls at midnight and the model
/// changes when the user switches one.
pub const ENVIRONMENT_PROMPT: &str = include_str!("prompts/environment.md");
pub const ENVIRONMENT_MARKER: &str = "# Environment";
/// How the environment block names the working directory, which is also how a
/// move is noticed: the block announced last names the directory left behind.
pub const WORKING_DIRECTORY_LABEL: &str = "- Working directory: ";
pub const CHECKOUT_SLOT: &str = "{checkout}";
/// Follows an environment whose working directory differs from the last one
/// announced, since paths from before the move still look valid.
pub const RELOCATED_PROMPT: &str = include_str!("prompts/relocated.md");
pub const RELOCATED_MARKER: &str = "# Working directory changed";
pub const FROM_SLOT: &str = "{from}";
pub const TO_SLOT: &str = "{to}";
/// Holds a handoff once while the todo list still has open items. Quotes the
/// whole list, not only what is open, because the model answers with a
/// replacement for all of it.
pub const OPEN_TODOS_PROMPT: &str = include_str!("prompts/open_todos.md");
pub const OPEN_TODOS_SLOT: &str = "{open}";
pub const TOTAL_TODOS_SLOT: &str = "{total}";
pub const TODOS_SLOT: &str = "{todos}";
/// Two fragments rather than one with a substituted clause, because the claim
/// that differs is the useful one: locally the scratch directory is where
/// `TMPDIR` already points and a command falls into it by itself, and on a
/// remote host it is a directory Caudra made that nothing else knows about.
pub const SCRATCH_LOCAL_PROMPT: &str = include_str!("prompts/scratch_local.md");
pub const SCRATCH_REMOTE_PROMPT: &str = include_str!("prompts/scratch_remote.md");
pub const SCRATCH_DIR_SLOT: &str = "{scratch_dir}";
/// Instruction files are snapshotted into the system prompt, so a later edit
/// arrives as a diff against that snapshot rather than by rebuilding it.
pub const INSTRUCTIONS_CHANGED_PROMPT: &str = include_str!("prompts/instructions_changed.md");
/// Carries the files whole rather than as a diff, for the session that started
/// with none: a patch against an empty snapshot is the text with every line
/// marked added, and the prompt it claims to patch says nothing at all.
pub const INSTRUCTIONS_APPEARED_PROMPT: &str = include_str!("prompts/instructions_appeared.md");
/// Withdraws an announced change once the files match the system prompt again.
/// Shares the heading, and so the kind, with the announcement it supersedes.
pub const INSTRUCTIONS_RESTORED_PROMPT: &str = include_str!("prompts/instructions_restored.md");
pub const INSTRUCTIONS_CHANGED_MARKER: &str = "# Instructions changed";
pub const DIFF_SLOT: &str = "{diff}";
pub const INSTRUCTIONS_SLOT: &str = "{instructions}";
/// Not a [`Vars`](crate::template::Vars) entry: the model is per-run rather
/// than a process-wide environment value, and is already threaded as `&Model`.
pub const MODEL_SLOT: &str = "{model}";
/// Headings of the two announcements. Emitting and detecting a mode share one
/// constant so history stays the source of truth.
pub const BUILD_MODE_MARKER: &str = "# Build Mode";
pub const PLAN_MODE_MARKER: &str = "# Plan Mode";
/// One kind between them: whichever was announced last is the mode in force.
pub(crate) const MODE_MARKERS: &[&str] = &[BUILD_MODE_MARKER, PLAN_MODE_MARKER];
pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");
pub const COMPACTION_MERGE: &str = include_str!("prompts/compaction_merge.md");
pub const GOAL_EVALUATOR: &str = include_str!("prompts/goal_evaluator.md");
pub const TITLE_SYSTEM: &str = include_str!("prompts/title.md");
pub const REQUIREMENTS_SYSTEM: &str = include_str!("prompts/requirements.md");
/// Carries [`TRANSCRIPT_SLOT`], filled with the user's side of the session.
pub const REQUIREMENTS_USER: &str = include_str!("prompts/requirements_user.md");
pub const TRANSCRIPT_SLOT: &str = "{transcript}";
/// Announced in the subagent's conversation for the same reason the interactive
/// modes are: the rule then sits closest to the point the model generates from.
/// Both modes share the heading, and so the kind, because a task's mode is
/// fixed when it opens and a continuation cannot change it.
pub const TASK_MODE_MARKER: &str = "# Host mode contract";
pub const TASK_PLAN_CONTRACT: &str = "<system-reminder>\n# Host mode contract\n\nYou are in plan mode. Inspect, reason, and report, but do not modify files, persistent state, or external systems. If implementation is needed, describe the exact changes without applying them.\n</system-reminder>";
pub const TASK_BUILD_CONTRACT: &str = "<system-reminder>\n# Host mode contract\n\nYou are in build mode. You may modify the workspace using the available tools. Complete the requested work, verify it, and report the result concisely.\n</system-reminder>";
/// Every kind of standing reminder, each named by the markers its blocks
/// carry, in the order a turn announces them. The latest block of a kind stays
/// in force until another replaces it, so a compaction that summarizes it away
/// has to restate it.
pub(crate) const STANDING_KINDS: &[&[&str]] = &[
    &[ENVIRONMENT_MARKER],
    &[INSTRUCTIONS_CHANGED_MARKER],
    &[TASK_MODE_MARKER],
    MODE_MARKERS,
];

const INSTRUCTIONS_MARKER: &str = "{{instructions}}";
const TASK_STYLE_HEADING: &str = "# Output discipline\n";
const TASK_TOOLS_HEADING: &str = "# Tool usage\n";
const RESEARCH_CONVENTIONS_HEADING: &str = "# Guidelines\n";
const GENERAL_CONVENTIONS_HEADING: &str = "# Conventions\n";
const GENERAL_COMPLETION_HEADING: &str = "# When done\n";
const CODE_MAP_TOOL_USAGE: &str = "- In an unfamiliar codebase, use **code_map** to see what matters before reading, **code_context** to find what a change touches, and **code_refs**/**code_impact** before editing a shared symbol. Their counts are floors: a zero means no reference was found, never that none exists.";
const INDEX_TOOL_USAGE: &str = "- Use the **file_index** tool first on individual files to get their skeleton, then use **file_read** with offset/limit for the specific section you need.";

/// `(tool, slot, content)`. Only applied when the tool survives the filter.
const NATIVE_HINTS: &[(&str, Slot, &str)] = &[
    ("code_map", Slot::ToolUsage, CODE_MAP_TOOL_USAGE),
    (
        crate::tools::FILE_INDEX_TOOL_NAME,
        Slot::ToolUsage,
        INDEX_TOOL_USAGE,
    ),
    (
        crate::tools::FILE_INDEX_TOOL_NAME,
        Slot::EfficientTools,
        crate::tools::FILE_INDEX_TOOL_NAME,
    ),
    (
        crate::tools::TODOWRITE_TOOL_NAME,
        Slot::ToolUsage,
        crate::tools::native::todo_write::TOOL_USAGE,
    ),
    (
        crate::tools::MEMORY_TOOL_NAME,
        Slot::ToolUsage,
        crate::tools::native::memory::TOOL_USAGE,
    ),
];

pub const DEFAULT_IDENTITY: &str = r#"You are Caudra, an interactive CLI coding agent. Use the tools available to assist the user with software engineering tasks. Complete tasks successfully while minimizing token usage and tool calls to avoid context bloat.

You must NEVER generate or guess URLs unless they are for helping the user with programming."#;

pub const DEFAULT_TONE: &str = r#"- Be concise. Your output is displayed on a CLI rendered in monospace. Use GitHub-flavored markdown.
- Only use emojis if explicitly requested.
- Do not add comments to code unless asked.
- Output text to communicate with the user; all text you output outside of tool use is displayed to the user. Only use tools to complete tasks. NEVER use shell commands to communicate thoughts, explanations, diagrams, or instructions to the user. Output all communication directly in your response text instead.
- NEVER create files unless absolutely necessary. ALWAYS prefer editing existing files."#;

const NATIVE_EFFICIENT_TOOLS: &[&str] = &[
    "batch",
    "file_grep",
    "file_edit",
    "file_apply_patch",
    "task",
];
const SYSTEM_COMPONENTS: &[&str] = &[
    "default",
    "identity",
    "style",
    "tools",
    "conventions",
    "completion",
    "context",
    "plan",
];

/// Singleton: alphabetically last plugin wins, discarding all prior content
/// and built-in defaults.  Used for slots with opinionated defaults where
/// multiple contributors would conflict (identity, tone).
///
/// Aggregate: all entries are joined.  Used for genuinely additive slots
/// where multiple plugins contributing is the point (tool usage hints,
/// efficient tools, after-instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display)]
#[strum(serialize_all = "snake_case")]
pub enum SlotKind {
    Singleton,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum Slot {
    Identity,
    Tone,
    ToolUsage,
    EfficientTools,
    Conventions,
    AfterInstructions,
}

impl Slot {
    fn marker(self) -> &'static str {
        match self {
            Slot::Identity => "{{identity}}",
            Slot::Tone => "{{tone}}",
            Slot::ToolUsage => "{{tool_usage}}",
            Slot::EfficientTools => "{{efficient_tools}}",
            Slot::Conventions => "{{conventions}}",
            Slot::AfterInstructions => "{{after_instructions}}",
        }
    }

    pub fn kind(self) -> SlotKind {
        match self {
            Slot::Identity | Slot::Tone => SlotKind::Singleton,
            Slot::ToolUsage
            | Slot::EfficientTools
            | Slot::Conventions
            | Slot::AfterInstructions => SlotKind::Aggregate,
        }
    }

    /// Built-in default content for singleton slots.  When no plugin
    /// registers content for a singleton slot, the default is used.
    /// Aggregate slots have no default (the template carries the static
    /// text around the marker).
    pub fn default_content(self) -> Option<&'static str> {
        match self {
            Slot::Identity => Some(DEFAULT_IDENTITY),
            Slot::Tone => Some(DEFAULT_TONE),
            _ => None,
        }
    }

    pub fn names_for_kind(kind: SlotKind) -> String {
        Self::iter()
            .filter(|s| s.kind() == kind)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum PromptId {
    System,
    Research,
    General,
}

impl PromptId {
    pub const ALL: &[PromptId] = &[PromptId::System, PromptId::Research, PromptId::General];
}

impl ValidNames for Slot {}
impl ValidNames for PromptId {}

#[derive(Clone)]
pub struct SlotEntry {
    pub plugin: Arc<str>,
    pub content: String,
}

#[derive(Clone, Default)]
pub struct ResolvedSlots {
    entries: HashMap<(PromptId, Slot), Vec<SlotEntry>>,
}

impl ResolvedSlots {
    pub fn get(&self, prompt: PromptId, slot: Slot) -> &[SlotEntry] {
        self.entries
            .get(&(prompt, slot))
            .map(|v| v.as_slice())
            .unwrap_or_default()
    }

    pub fn insert(&mut self, prompt: PromptId, slot: Slot, entry: SlotEntry) {
        self.entries.entry((prompt, slot)).or_default().push(entry);
    }

    pub fn with_execution_guidance(&self, guidance: &str) -> Self {
        let mut slots = self.clone();
        for &prompt in PromptId::ALL {
            let entries = slots.entries.entry((prompt, Slot::ToolUsage)).or_default();
            entries.retain(|entry| entry.plugin.as_ref() != EXECUTION_HINT_OWNER);
            if !guidance.is_empty() {
                entries.push(SlotEntry {
                    plugin: Arc::from(EXECUTION_HINT_OWNER),
                    content: guidance.into(),
                });
            }
        }
        slots
    }

    /// Native tools cannot register prompt hints the way Lua plugins do: they
    /// register once at startup, long before a prompt exists, and the same
    /// registry serves filters that exclude them. Applying the hints here ties
    /// each one to its tool actually being offered.
    pub fn with_native_hints(&self, filter: &crate::tools::ToolFilter) -> Cow<'_, Self> {
        self.with_native_hints_and_memory(filter, None)
    }

    pub fn with_native_hints_for_store<'a>(
        &'a self,
        filter: &crate::tools::ToolFilter,
        store: &'a caudra_storage::local_documents::LocalDocumentStore,
    ) -> Cow<'a, Self> {
        self.with_native_hints_and_memory(filter, Some(store))
    }

    fn with_native_hints_and_memory<'a>(
        &'a self,
        filter: &crate::tools::ToolFilter,
        store: Option<&caudra_storage::local_documents::LocalDocumentStore>,
    ) -> Cow<'a, Self> {
        let hints: Vec<_> = NATIVE_HINTS
            .iter()
            .filter(|(tool, ..)| filter.matches(tool))
            .collect();
        if hints.is_empty() && !filter.matches(crate::tools::MEMORY_TOOL_NAME) {
            return Cow::Borrowed(self);
        }
        let mut slots = self.clone();
        for &prompt in PromptId::ALL {
            for (tool, slot, content) in &hints {
                slots.insert(
                    prompt,
                    *slot,
                    SlotEntry {
                        plugin: Arc::from(format!("native:{tool}")),
                        content: (*content).into(),
                    },
                );
            }
        }
        // The memory tag index is scanned from disk, so unlike the fixed hints
        // it cannot live in a const table. It only reaches the system prompt:
        // a subagent gets the tool, not the whole project's tag vocabulary.
        let memory_line = store.map_or_else(
            crate::tools::native::memory::prompt_tag_line_for_cwd,
            crate::tools::native::memory::prompt_tag_line_for_store,
        );
        if filter.matches(crate::tools::MEMORY_TOOL_NAME)
            && let Some(line) = memory_line
        {
            slots.insert(
                PromptId::System,
                Slot::AfterInstructions,
                SlotEntry {
                    plugin: Arc::from("native:memory"),
                    content: line,
                },
            );
        }
        Cow::Owned(slots)
    }
}

impl PromptId {
    fn template(self) -> &'static str {
        match self {
            PromptId::System => SYSTEM_PROMPT,
            PromptId::Research => RESEARCH_PROMPT,
            PromptId::General => GENERAL_PROMPT,
        }
    }

    /// A slot exists for this prompt iff its marker is present in the template.
    /// Markers that are absent get no content (and we warn at collection time
    /// when a plugin targets them explicitly).
    pub fn has_slot(self, slot: Slot) -> bool {
        match self {
            PromptId::System => true,
            PromptId::Research | PromptId::General => self.template().contains(slot.marker()),
        }
    }
}

fn render_slot(slots: &ResolvedSlots, prompt: PromptId, slot: Slot) -> String {
    if slot == Slot::EfficientTools {
        return render_efficient_tools(slots, prompt);
    }
    let entries = slots.get(prompt, slot);
    match slot.kind() {
        SlotKind::Singleton => {
            if let Some(last) = entries.last() {
                last.content.clone()
            } else if let Some(default) = slot.default_content() {
                default.to_string()
            } else {
                String::new()
            }
        }
        // Aggregate slots have no built-in defaults; content comes entirely from plugins.
        SlotKind::Aggregate => {
            let mut parts = Vec::new();
            for entry in entries {
                parts.push(entry.content.as_str());
            }
            parts.join("\n")
        }
    }
}

fn render_efficient_tools(slots: &ResolvedSlots, prompt: PromptId) -> String {
    let extras = slots.get(prompt, Slot::EfficientTools);
    let names = NATIVE_EFFICIENT_TOOLS
        .iter()
        .copied()
        .filter(|name| prompt == PromptId::System || *name != "task")
        .chain(extras.iter().map(|e| e.content.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Most efficient tools: {names}.")
}

pub(crate) fn is_system_component(name: &str) -> bool {
    SYSTEM_COMPONENTS.contains(&name)
}

struct SystemParts {
    identity: String,
    style: String,
    tools: String,
    conventions: String,
    completion: String,
    context: String,
    plan: String,
}

impl SystemParts {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "identity" => Some(&self.identity),
            "style" => Some(&self.style),
            "tools" => Some(&self.tools),
            "conventions" => Some(&self.conventions),
            "completion" => Some(&self.completion),
            "context" => Some(&self.context),
            "plan" => Some(&self.plan),
            _ => None,
        }
    }
}

fn render_lines<'a>(template: &str, mut resolve: impl FnMut(&str) -> Option<&'a str>) -> String {
    let mut output = String::with_capacity(template.len());
    for line in template.split_inclusive('\n') {
        let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = line.strip_suffix('\n') {
            (body, "\n")
        } else {
            (line, "")
        };
        if let Some(content) = resolve(body) {
            if !content.is_empty() {
                output.push_str(content);
                output.push_str(ending);
            }
        } else {
            output.push_str(line);
        }
    }
    output
}

fn render_prompt_template(
    template: &str,
    slots: &ResolvedSlots,
    prompt: PromptId,
    instructions: &str,
) -> String {
    let rendered_slots = Slot::iter()
        .map(|slot| (slot.marker(), render_slot(slots, prompt, slot)))
        .collect::<Vec<_>>();
    render_lines(template, |marker| {
        if marker == "{{instructions}}" {
            return Some(instructions);
        }
        rendered_slots
            .iter()
            .find_map(|(candidate, content)| (*candidate == marker).then_some(content.as_str()))
    })
}

fn system_parts(slots: &ResolvedSlots, instructions: &str, plan: &str) -> SystemParts {
    SystemParts {
        identity: render_slot(slots, PromptId::System, Slot::Identity),
        style: remove_template_line_ending(render_prompt_template(
            SYSTEM_STYLE,
            slots,
            PromptId::System,
            "",
        )),
        tools: remove_template_line_ending(render_prompt_template(
            SYSTEM_TOOLS,
            slots,
            PromptId::System,
            "",
        )),
        conventions: remove_template_line_ending(render_prompt_template(
            SYSTEM_CONVENTIONS,
            slots,
            PromptId::System,
            "",
        )),
        completion: remove_template_line_ending(SYSTEM_COMPLETION.to_owned()),
        context: format!(
            "{}{}",
            instructions,
            render_slot(slots, PromptId::System, Slot::AfterInstructions)
        ),
        plan: plan.to_owned(),
    }
}

fn remove_template_line_ending(mut rendered: String) -> String {
    let bytes = rendered.as_bytes();
    let ending_bytes = if bytes.ends_with(b"\r\n") {
        2
    } else if bytes.ends_with(b"\n") {
        1
    } else {
        0
    };
    rendered.truncate(rendered.len() - ending_bytes);
    rendered
}

fn render_system_layout(template: &str, parts: &SystemParts) -> String {
    render_lines(template, |marker| {
        let name = marker
            .strip_prefix("{{caudra.")
            .and_then(|marker| marker.strip_suffix("}}"))?;
        parts.get(name)
    })
}

fn render_custom_layout<'a>(
    template: &str,
    default: &str,
    mut resolve: impl FnMut(&str) -> Option<&'a str>,
) -> String {
    let mut output = String::with_capacity(template.len() + default.len());
    for line in template.split_inclusive('\n') {
        let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = line.strip_suffix('\n') {
            (body, "\n")
        } else {
            (line, "")
        };
        if let Some(literal) = body.strip_prefix("\\{{caudra.")
            && literal.ends_with("}}")
        {
            output.push_str("{{caudra.");
            output.push_str(literal);
            output.push_str(ending);
            continue;
        }
        let component = body
            .strip_prefix("{{caudra.")
            .and_then(|body| body.strip_suffix("}}"));
        let content = match component {
            Some("default") => Some(default),
            Some(name) => resolve(name),
            None => None,
        };
        if let Some(content) = content {
            if !content.is_empty() {
                output.push_str(content);
                if !content.ends_with('\n') {
                    output.push_str(ending);
                }
            }
        } else {
            output.push_str(line);
        }
    }
    output
}

pub fn assemble_system(
    slots: &ResolvedSlots,
    instructions: &str,
    plan: &str,
    profile: Option<&SystemPromptProfile>,
) -> String {
    let parts = system_parts(slots, instructions, plan);
    let mut default = render_system_layout(SYSTEM_PROMPT.trim_end_matches(['\r', '\n']), &parts);
    let Some(profile) = profile else {
        default.push_str(&parts.plan);
        return default;
    };
    match profile.layout() {
        PromptProfileLayout::Overlay => {
            default.push_str("\n\n");
            default.push_str(profile.body());
            default.push_str(&parts.plan);
            default
        }
        PromptProfileLayout::Custom => {
            default.push_str(&parts.plan);
            render_custom_layout(profile.body(), &default, |name| parts.get(name))
        }
    }
}

fn task_parts(slots: &ResolvedSlots, mode: PromptId, instructions: &str) -> Option<SystemParts> {
    let template = mode.template();
    let style_start = template.find(TASK_STYLE_HEADING)?;
    let tools_start = template.find(TASK_TOOLS_HEADING)?;
    let conventions_heading = match mode {
        PromptId::Research => RESEARCH_CONVENTIONS_HEADING,
        PromptId::General => GENERAL_CONVENTIONS_HEADING,
        PromptId::System => return None,
    };
    let conventions_start = template.find(conventions_heading)?;
    let instructions_start = template.rfind(INSTRUCTIONS_MARKER)?;
    let completion_start = match mode {
        PromptId::Research => instructions_start,
        PromptId::General => template.find(GENERAL_COMPLETION_HEADING)?,
        PromptId::System => return None,
    };
    if ![
        style_start,
        tools_start,
        conventions_start,
        completion_start,
        instructions_start,
    ]
    .is_sorted()
    {
        return None;
    }

    let render = |fragment: &str| {
        render_prompt_template(fragment.trim_matches(['\r', '\n']), slots, mode, "")
    };
    Some(SystemParts {
        identity: render(&template[..style_start]),
        style: render(&template[style_start..tools_start]),
        tools: render(&template[tools_start..conventions_start]),
        conventions: render(&template[conventions_start..completion_start]),
        completion: render(&template[completion_start..instructions_start]),
        context: instructions.to_owned(),
        plan: String::new(),
    })
}

/// Assemble a research or general task prompt under a system prompt profile.
/// The reminder contract is opaque to templates and is appended exactly once,
/// last, so a profile cannot leave a subagent unable to read its own
/// announcements. What varies between runs is announced instead of assembled:
/// the environment, and the mode the host granted this task.
pub fn assemble_task(
    mode: PromptId,
    slots: &ResolvedSlots,
    instructions: &str,
    profile: Option<&SystemPromptProfile>,
) -> String {
    let mut default = render_prompt_template(mode.template(), slots, mode, instructions);
    let mut output = match profile {
        None => default,
        Some(profile) if profile.layout() == PromptProfileLayout::Overlay => {
            if default.ends_with('\n') {
                default.push('\n');
            } else {
                default.push_str("\n\n");
            }
            default.push_str(profile.body());
            default
        }
        Some(profile) => match task_parts(slots, mode, instructions) {
            Some(parts) => render_custom_layout(profile.body(), &default, |name| parts.get(name)),
            None => default,
        },
    };
    output.push_str(REMINDERS_PROMPT);
    output
}

pub fn assemble_task_with_filter(
    mode: PromptId,
    slots: &ResolvedSlots,
    filter: &crate::tools::ToolFilter,
    instructions: &str,
    profile: Option<&SystemPromptProfile>,
) -> String {
    assemble_task(
        mode,
        &slots.with_native_hints(filter),
        instructions,
        profile,
    )
}

/// Fill each host template marker once. Inserted content is opaque and is not
/// scanned again for markers.
pub fn assemble(id: PromptId, slots: &ResolvedSlots, instructions: &str) -> String {
    if id == PromptId::System {
        return assemble_system(slots, instructions, "", None);
    }
    render_prompt_template(id.template(), slots, id, instructions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const NATIVE_EFFICIENT_LINE: &str =
        "Most efficient tools: batch, file_grep, file_edit, file_apply_patch, task";
    /// Subagent prompts drop `task`, which only the main agent may call.
    const SUBAGENT_EFFICIENT_LINE: &str =
        "Most efficient tools: batch, file_grep, file_edit, file_apply_patch";
    const EXECUTION_TEST_THRESHOLD: u64 = 937;
    const CUSTOM_EXECUTION_INSTRUCTION: &str =
        "User-authored background and foreground instructions remain intact.";

    #[test_case(ExecutionMode::Sync)]
    #[test_case(ExecutionMode::Auto)]
    #[test_case(ExecutionMode::Async)]
    fn composed_execution_guidance_respects_independent_modes(task_mode: ExecutionMode) {
        for shell_mode in [
            ExecutionMode::Sync,
            ExecutionMode::Auto,
            ExecutionMode::Async,
        ] {
            let config = AgentConfig {
                task_execution: task_mode.clone(),
                shell_execution: shell_mode.clone(),
                shell_async_threshold_secs: EXECUTION_TEST_THRESHOLD,
                ..AgentConfig::default()
            };
            let guidance = execution_guidance(&config, true, true, true, true);
            let slots = ResolvedSlots::default().with_execution_guidance(&guidance);
            for prompt in [PromptId::System, PromptId::Research, PromptId::General] {
                let rendered = if prompt == PromptId::System {
                    assemble_system(&slots, "", STANDING_PROMPT, None)
                } else {
                    assemble_task(prompt, &slots, "", None)
                };
                assert!(rendered.contains(&task_execution_guidance(&task_mode)));
                assert!(rendered.contains(&shell_execution_guidance(
                    &shell_mode,
                    EXECUTION_TEST_THRESHOLD
                )));
                assert_eq!(
                    rendered.contains(&EXECUTION_TEST_THRESHOLD.to_string()),
                    shell_mode == ExecutionMode::Auto
                );
                if task_mode == ExecutionMode::Sync && shell_mode == ExecutionMode::Sync {
                    for forbidden in ["background", "async", "receipt", "promote"] {
                        assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
                    }
                }
                if task_mode == ExecutionMode::Async && shell_mode == ExecutionMode::Async {
                    for forbidden in ["foreground", "synchronous", "background: false", "promote"] {
                        assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
                    }
                }
            }
        }
    }

    #[test_case(false, false; "neither_tool")]
    #[test_case(true, false; "task_only")]
    #[test_case(false, true; "shell_only_child")]
    #[test_case(true, true; "both_tools")]
    fn execution_guidance_only_exposes_supported_tools(task_exposed: bool, shell_exposed: bool) {
        let mut config = AgentConfig::default();
        let guidance = execution_guidance(&config, false, false, task_exposed, shell_exposed);
        assert_eq!(guidance.contains("Task calls"), task_exposed);
        assert_eq!(guidance.contains("Shell calls"), shell_exposed);
        for forbidden in ["background", "async", "receipt", "promote"] {
            assert!(!guidance.contains(forbidden));
        }
        config.task_execution = ExecutionMode::Async;
        config.shell_execution = ExecutionMode::Async;
        assert!(execution_guidance(&config, false, false, task_exposed, shell_exposed).is_empty());
    }

    #[test_case(PromptId::System)]
    #[test_case(PromptId::Research)]
    #[test_case(PromptId::General)]
    fn execution_guidance_refresh_preserves_custom_instructions(prompt: PromptId) {
        let base = slots(prompt, &[(Slot::ToolUsage, CUSTOM_EXECUTION_INSTRUCTION)]);
        let configured =
            base.with_execution_guidance(&task_execution_guidance(&ExecutionMode::Async));
        let refreshed =
            configured.with_execution_guidance(&task_execution_guidance(&ExecutionMode::Sync));
        let rendered = assemble(prompt, &refreshed, "");
        assert!(rendered.contains(CUSTOM_EXECUTION_INSTRUCTION));
        assert!(!rendered.contains(TASK_ASYNC_GUIDANCE));
        assert_eq!(rendered.matches(TASK_SYNC_GUIDANCE).count(), 1);
        assert_eq!(
            assemble(prompt, &refreshed.with_execution_guidance(""), ""),
            assemble(prompt, &base, "")
        );
    }

    #[test_case(COMPACTION_SYSTEM)]
    #[test_case(COMPACTION_USER)]
    #[test_case(COMPACTION_MERGE)]
    fn compaction_guidance_does_not_advertise_execution_modes(template: &str) {
        for forbidden in ["background", "async", "foreground", "receipt"] {
            assert!(!template.contains(forbidden));
        }
    }

    fn slots(prompt: PromptId, entries: &[(Slot, &str)]) -> ResolvedSlots {
        let mut slots = ResolvedSlots::default();
        for &(slot, content) in entries {
            slots.insert(
                prompt,
                slot,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: content.into(),
                },
            );
        }
        slots
    }

    fn at(out: &str, needle: &str) -> usize {
        out.find(needle)
            .unwrap_or_else(|| panic!("missing: {needle}"))
    }

    #[test]
    fn empty_slots_emit_template_and_native_efficient_line() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Caudra"));
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}.")));
    }

    /// One test to pin the whole System layout: every slot shows up, in order,
    /// around the instructions. Covers presence and ordering for all of them.
    #[test]
    fn system_sections_land_in_layout_order() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::ToolUsage, "TOOL_USAGE"),
                (Slot::EfficientTools, "EXTRA_TOOL"),
                (Slot::Conventions, "CONVENTIONS"),
                (Slot::AfterInstructions, "AFTER"),
            ],
        );
        let out = assemble(PromptId::System, &s, "INSTR");
        let positions = ["TOOL_USAGE", "EXTRA_TOOL", "CONVENTIONS", "INSTR", "AFTER"]
            .map(|needle| at(&out, needle));
        assert!(
            positions.is_sorted(),
            "sections out of layout order ({positions:?}):\n{out}"
        );
    }

    /// Regression: a `tool_usage` hint must land inside the `# Tool usage`
    /// section, not be appended after the rest of the prompt.
    #[test]
    fn tool_usage_hint_lands_inside_tool_usage_section() {
        const HINT: &str = "- HINT_LINE";
        let s = slots(PromptId::System, &[(Slot::ToolUsage, HINT)]);
        let out = assemble(PromptId::System, &s, "");
        let hint = at(&out, HINT);
        assert!(
            at(&out, "# Tool usage") < hint,
            "hint before its section:\n{out}"
        );
        assert!(
            hint < at(&out, "# Conventions"),
            "hint leaked past section:\n{out}"
        );
    }

    #[test]
    fn efficient_tools_extras_join_native_list() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::EfficientTools, "file_index"),
                (Slot::EfficientTools, "foo"),
            ],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, file_index, foo.")));
    }

    #[test_case(crate::tools::ToolFilter::All, true ; "enabled")]
    #[test_case(crate::tools::ToolFilter::AllExcept(vec!["file_index".into()]), false ; "disabled")]
    fn native_index_hints_follow_effective_filter(
        filter: crate::tools::ToolFilter,
        expected: bool,
    ) {
        let slots = ResolvedSlots::default();
        let filtered = slots.with_native_hints(&filter);
        let output = assemble(PromptId::System, &filtered, "");
        assert_eq!(output.contains(INDEX_TOOL_USAGE), expected);
        assert_eq!(output.contains("task, file_index."), expected);
    }

    #[test_case(PromptId::Research ; "research")]
    #[test_case(PromptId::General ; "general")]
    fn task_prompts_do_not_recommend_the_main_only_task_tool(prompt: PromptId) {
        let out = assemble(prompt, &ResolvedSlots::default(), "");
        assert!(out.contains(&format!("{SUBAGENT_EFFICIENT_LINE}.")));
        assert!(!out.contains(NATIVE_EFFICIENT_LINE));
    }

    #[test]
    fn same_slot_preserves_insertion_order() {
        let s = slots(
            PromptId::System,
            &[(Slot::ToolUsage, "FIRST"), (Slot::ToolUsage, "SECOND")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(at(&out, "FIRST") < at(&out, "SECOND"));
    }

    /// Only System carries AfterInstructions, so the same content shows up there
    /// but never leaks into the subagent prompts.
    #[test]
    fn after_instructions_only_reaches_system() {
        let mut s = ResolvedSlots::default();
        for &pid in PromptId::ALL {
            s.insert(
                pid,
                Slot::AfterInstructions,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: "AFTER".into(),
                },
            );
        }
        assert!(assemble(PromptId::System, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::Research, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::General, &s, "").contains("AFTER"));
    }

    #[test]
    fn research_drops_conventions_but_keeps_efficient_extras() {
        let s = slots(
            PromptId::Research,
            &[
                (Slot::Conventions, "DROPPED"),
                (Slot::EfficientTools, "EXTRA"),
            ],
        );
        let out = assemble(PromptId::Research, &s, "");
        assert!(!out.contains("DROPPED"));
        assert!(out.contains(&format!("{SUBAGENT_EFFICIENT_LINE}, EXTRA.")));
    }

    #[test_case(PromptId::System, Slot::ToolUsage, true ; "system_tool_usage")]
    #[test_case(PromptId::System, Slot::EfficientTools, true ; "system_efficient")]
    #[test_case(PromptId::System, Slot::Conventions, true ; "system_conventions")]
    #[test_case(PromptId::System, Slot::AfterInstructions, true ; "system_after")]
    #[test_case(PromptId::System, Slot::Identity, true ; "system_identity")]
    #[test_case(PromptId::System, Slot::Tone, true ; "system_tone")]
    #[test_case(PromptId::Research, Slot::Conventions, false ; "research_no_conventions")]
    #[test_case(PromptId::Research, Slot::AfterInstructions, false ; "research_no_after")]
    #[test_case(PromptId::Research, Slot::Identity, false ; "research_no_identity")]
    #[test_case(PromptId::Research, Slot::Tone, false ; "research_no_tone")]
    #[test_case(PromptId::General, Slot::AfterInstructions, false ; "general_no_after")]
    #[test_case(PromptId::General, Slot::Identity, false ; "general_no_identity")]
    #[test_case(PromptId::General, Slot::Tone, false ; "general_no_tone")]
    fn has_slot(prompt: PromptId, slot: Slot, expected: bool) {
        assert_eq!(prompt.has_slot(slot), expected);
    }

    #[test_case("after_instructions", Some(Slot::AfterInstructions) ; "valid_slot")]
    #[test_case("tool_usagee", None ; "typo_slot")]
    #[test_case("identity", Some(Slot::Identity) ; "identity_slot")]
    #[test_case("tone", Some(Slot::Tone) ; "tone_slot")]
    fn slot_parse_is_plugin_contract(input: &str, expected: Option<Slot>) {
        assert_eq!(input.parse::<Slot>().ok(), expected);
    }

    #[test_case("system", Some(PromptId::System) ; "valid_prompt")]
    #[test_case("systm", None ; "typo_prompt")]
    fn prompt_parse_is_plugin_contract(input: &str, expected: Option<PromptId>) {
        assert_eq!(input.parse::<PromptId>().ok(), expected);
    }

    #[test_case(Slot::Identity, SlotKind::Singleton ; "identity_singleton")]
    #[test_case(Slot::Tone, SlotKind::Singleton ; "tone_singleton")]
    #[test_case(Slot::Conventions, SlotKind::Aggregate ; "conventions_aggregate")]
    #[test_case(Slot::ToolUsage, SlotKind::Aggregate ; "tool_usage_aggregate")]
    #[test_case(Slot::EfficientTools, SlotKind::Aggregate ; "efficient_aggregate")]
    #[test_case(Slot::AfterInstructions, SlotKind::Aggregate ; "after_aggregate")]
    fn slot_kind_matches_expectations(slot: Slot, expected: SlotKind) {
        assert_eq!(slot.kind(), expected);
    }

    #[test]
    fn singleton_default_used_when_empty() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Caudra"));
    }

    #[test]
    fn singleton_entry_replaces_default() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("user"),
                content: "Custom identity".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Custom identity"));
        assert!(!out.contains("You are Caudra"));
    }

    #[test]
    fn singleton_last_entry_wins() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("first"),
                content: "FIRST".into(),
            },
        );
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("second"),
                content: "SECOND".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("SECOND"));
        assert!(!out.contains("FIRST"));
        assert!(!out.contains("You are Caudra"));
    }

    #[test]
    fn identity_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Identity));
        assert!(!PromptId::Research.has_slot(Slot::Identity));
        assert!(!PromptId::General.has_slot(Slot::Identity));
    }

    #[test]
    fn tone_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Tone));
        assert!(!PromptId::Research.has_slot(Slot::Tone));
        assert!(!PromptId::General.has_slot(Slot::Tone));
    }

    #[test]
    fn conventions_entry_appends_to_template_defaults() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Conventions,
            SlotEntry {
                plugin: Arc::from("plugin"),
                content: "- Extra rule".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Never assume a library is available"));
        assert!(out.contains("- Extra rule"));
    }

    #[test]
    fn inserted_content_is_not_scanned_for_markers() {
        let mut slots = ResolvedSlots::default();
        slots.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("plugin"),
                content: "Literal {{tone}} and {{instructions}}".into(),
            },
        );
        let output = assemble(PromptId::System, &slots, "RUNTIME");
        assert!(output.contains("Literal {{tone}} and {{instructions}}"));
        assert!(output.contains("RUNTIME"));
    }

    #[test]
    fn built_in_context_and_plan_keep_legacy_boundaries() {
        let slots = slots(PromptId::System, &[(Slot::AfterInstructions, "AFTER")]);

        let build = assemble_system(&slots, "INSTRUCTIONS", "", None);
        assert!(build.ends_with("INSTRUCTIONSAFTER"));

        let plan = assemble_system(&slots, "INSTRUCTIONS", "\n\nPLAN", None);
        assert!(plan.ends_with("INSTRUCTIONSAFTER\n\nPLAN"));
    }

    #[test]
    fn newline_terminated_slots_keep_template_spacing() {
        let slots = slots(PromptId::System, &[(Slot::Conventions, "EXTRA\n")]);

        let output = assemble_system(&slots, "", "", None);
        assert!(output.contains("EXTRA\n\n\n# When done"));
    }

    #[test_case(PromptId::Research, "You are a research agent" ; "research")]
    #[test_case(PromptId::General, "You are a general-purpose coding agent" ; "general")]
    fn builtin_task_uses_existing_default_and_appends_the_reminder_contract_last(
        mode: PromptId,
        identity: &str,
    ) {
        let slots = ResolvedSlots::default();
        let default = assemble(mode, &slots, "TASK_CONTEXT");

        let output = assemble_task(mode, &slots, "TASK_CONTEXT", None);
        assert_eq!(output, format!("{default}{REMINDERS_PROMPT}"));
        assert!(output.starts_with(identity));
        assert!(output.ends_with(REMINDERS_PROMPT));
    }

    /// The task prompt precedes every message a subagent sends, so anything
    /// here that varies re-caches its whole session when it moves. The mode and
    /// the environment are announced in the conversation instead.
    #[test_case(ENVIRONMENT_MARKER ; "environment_stays_out")]
    #[test_case("Working directory" ; "cwd_stays_out")]
    #[test_case(TASK_MODE_MARKER ; "mode_contract_stays_out")]
    #[test_case(MODEL_SLOT ; "model_stays_out")]
    fn a_task_prompt_carries_nothing_that_varies(absent: &str) {
        for mode in [PromptId::Research, PromptId::General] {
            let output = assemble_task(mode, &ResolvedSlots::default(), "", None);
            assert!(!output.contains(absent), "{mode} carries {absent}");
        }
    }

    /// Announcements arrive as user-role observations, so without this the tag
    /// is the only thing telling a subagent they are not its caller talking.
    #[test_case(PromptId::Research ; "research")]
    #[test_case(PromptId::General ; "general")]
    fn a_task_prompt_explains_the_reminder_contract(mode: PromptId) {
        let output = assemble_task(mode, &ResolvedSlots::default(), "", None);
        assert!(output.contains(REMINDERS_PROMPT.trim_end()));
    }
}
