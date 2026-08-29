use std::collections::HashMap;
use std::sync::Arc;

use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

pub mod profile;

use profile::{PromptProfileLayout, SystemPromptProfile};

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
pub const PLAN_PROMPT: &str = include_str!("prompts/plan.md");
pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");
pub const GOAL_EVALUATOR: &str = include_str!("prompts/goal_evaluator.md");

pub const DEFAULT_IDENTITY: &str = r#"You are Maki, an interactive CLI coding agent. Use the tools available to assist the user with software engineering tasks. Complete tasks successfully while minimizing token usage and tool calls to avoid context bloat.

You must NEVER generate or guess URLs unless they are for helping the user with programming."#;

pub const DEFAULT_TONE: &str = r#"- Be concise. Your output is displayed on a CLI rendered in monospace. Use GitHub-flavored markdown.
- Only use emojis if explicitly requested.
- Do not add comments to code unless asked.
- Output text to communicate with the user; all text you output outside of tool use is displayed to the user. Only use tools to complete tasks. NEVER use bash echo or other command-line tools to communicate thoughts, explanations, diagrams, or instructions to the user. Output all communication directly in your response text instead.
- NEVER create files unless absolutely necessary. ALWAYS prefer editing existing files."#;

const NATIVE_EFFICIENT_TOOLS: &[&str] = &["batch", "code_execution", "task"];
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

pub struct SlotEntry {
    pub plugin: Arc<str>,
    pub content: String,
}

#[derive(Default)]
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
            .strip_prefix("{{maki.")
            .and_then(|marker| marker.strip_suffix("}}"))?;
        parts.get(name)
    })
}

fn render_custom_layout(template: &str, parts: &SystemParts, default: &str) -> String {
    let mut output = String::with_capacity(template.len() + default.len());
    for line in template.split_inclusive('\n') {
        let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = line.strip_suffix('\n') {
            (body, "\n")
        } else {
            (line, "")
        };
        if let Some(literal) = body.strip_prefix("\\{{maki.")
            && literal.ends_with("}}")
        {
            output.push_str("{{maki.");
            output.push_str(literal);
            output.push_str(ending);
            continue;
        }
        let component = body
            .strip_prefix("{{maki.")
            .and_then(|body| body.strip_suffix("}}"));
        let content = match component {
            Some("default") => Some(default),
            Some(name) => parts.get(name),
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
            render_custom_layout(profile.body(), &parts, &default)
        }
    }
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

    const NATIVE_EFFICIENT_LINE: &str = "Most efficient tools: batch, code_execution, task";

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
        assert!(out.starts_with("You are Maki"));
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
                (Slot::EfficientTools, "index"),
                (Slot::EfficientTools, "foo"),
            ],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, index, foo.")));
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
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, EXTRA.")));
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
        assert!(out.starts_with("You are Maki"));
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
        assert!(!out.contains("You are Maki"));
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
        assert!(!out.contains("You are Maki"));
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
}
