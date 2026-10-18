use std::collections::HashSet;

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};
use caudra_agent::McpPromptInfo;
use caudra_agent::agent::COMPACT_DESCRIPTION;
use caudra_agent::mcp::McpHandle;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::skill::{self, SkillInventoryEntry};

pub(crate) const COMPACT_COMMAND: &str = "compact";
const COMMAND_PREFIX: char = '/';
const SKILL_HINT: &str = "optional instructions for the skill";

/// The slash commands one session offers, read from what its skill tool and
/// MCP servers hold now.
#[derive(Default)]
pub(crate) struct CommandCatalog {
    pub(crate) skills: Vec<SkillInventoryEntry>,
    pub(crate) prompts: Vec<McpPromptInfo>,
}

/// A prompt that names an offered command, and what it asked of it.
pub(crate) enum Invocation<'a> {
    Compact,
    Skill(&'a str),
    McpPrompt {
        prompt: &'a McpPromptInfo,
        args: &'a str,
    },
}

enum Command<'a> {
    Compact,
    Skill(&'a SkillInventoryEntry),
    McpPrompt(&'a McpPromptInfo),
}

impl Command<'_> {
    fn description(&self) -> &str {
        match self {
            Self::Compact => COMPACT_DESCRIPTION,
            Self::Skill(skill) => &skill.description,
            Self::McpPrompt(prompt) => &prompt.description,
        }
    }

    fn hint(&self) -> Option<String> {
        match self {
            Self::Compact => None,
            Self::Skill(_) => Some(SKILL_HINT.to_owned()),
            Self::McpPrompt(prompt) => (!prompt.arguments.is_empty()).then(|| {
                prompt
                    .arguments
                    .iter()
                    .map(|arg| {
                        if arg.required {
                            format!("<{}>", arg.name)
                        } else {
                            format!("[{}]", arg.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        }
    }
}

impl CommandCatalog {
    pub(crate) fn of(registry: &ToolRegistry, mcp: Option<&McpHandle>) -> Self {
        Self {
            skills: skill::inventory(registry),
            prompts: mcp
                .map(|mcp| mcp.reader().load().prompts.clone())
                .unwrap_or_default(),
        }
    }

    /// Every command once, a built-in before a skill before an MCP prompt
    /// when two share a name.
    fn entries(&self) -> Vec<(&str, Command<'_>)> {
        let skills = self
            .skills
            .iter()
            .map(|skill| (skill.name.as_str(), Command::Skill(skill)));
        let prompts = self
            .prompts
            .iter()
            .map(|prompt| (prompt.display_name.as_str(), Command::McpPrompt(prompt)));
        let mut seen = HashSet::new();
        [(COMPACT_COMMAND, Command::Compact)]
            .into_iter()
            .chain(skills)
            .chain(prompts)
            .filter(|(name, _)| seen.insert(*name))
            .collect()
    }

    pub(crate) fn commands(&self) -> Vec<AvailableCommand> {
        self.entries()
            .into_iter()
            .map(|(name, command)| {
                let input = command.hint().map(|hint| {
                    AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(hint))
                });
                AvailableCommand::new(name, command.description()).input(input)
            })
            .collect()
    }

    /// `None` for text that names no offered command, which goes to the
    /// model as typed.
    pub(crate) fn route<'a>(&'a self, text: &'a str) -> Option<Invocation<'a>> {
        let (name, args) = parse_command(text)?;
        let (_, command) = self
            .entries()
            .into_iter()
            .find(|(offered, _)| *offered == name)?;
        Some(match command {
            Command::Compact => Invocation::Compact,
            Command::Skill(skill) => Invocation::Skill(&skill.name),
            Command::McpPrompt(prompt) => Invocation::McpPrompt { prompt, args },
        })
    }
}

/// Splits `/name args` into the name and the trimmed rest.
fn parse_command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix(COMMAND_PREFIX)?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then_some((name, args.trim()))
}

#[cfg(test)]
mod tests {
    use caudra_agent::McpPromptArg;
    use caudra_agent::tools::native::skill::SkillScope;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    const SKILL: &str = "review";
    const SKILL_DESCRIPTION: &str = "Review a change";
    const PROMPT: &str = "server:summarize";
    const PROMPT_DESCRIPTION: &str = "Summarize text";
    const REQUIRED_ARG: &str = "text";
    const OPTIONAL_ARG: &str = "style";
    const ARGS: &str = "src/lib.rs and more";
    const UNKNOWN: &str = "unknown";
    const SKILL_KIND: &str = "skill";
    const PROMPT_KIND: &str = "prompt";

    fn skill(name: &str) -> SkillInventoryEntry {
        SkillInventoryEntry {
            name: name.into(),
            description: SKILL_DESCRIPTION.into(),
            location: String::new(),
            scope: SkillScope::Project,
        }
    }

    fn prompt(name: &str) -> McpPromptInfo {
        McpPromptInfo {
            display_name: name.into(),
            qualified_name: name.into(),
            description: PROMPT_DESCRIPTION.into(),
            arguments: [(REQUIRED_ARG, true), (OPTIONAL_ARG, false)]
                .into_iter()
                .map(|(name, required)| McpPromptArg {
                    name: name.into(),
                    description: String::new(),
                    required,
                })
                .collect(),
            completions: false,
        }
    }

    fn catalog() -> CommandCatalog {
        CommandCatalog {
            skills: vec![skill(SKILL), skill(COMPACT_COMMAND)],
            prompts: vec![prompt(PROMPT), prompt(SKILL)],
        }
    }

    #[test]
    fn commands_list_each_name_once_with_its_input_hint() {
        let listed = serde_json::to_value(catalog().commands()).unwrap();
        assert_eq!(
            listed,
            json!([
                {"name": COMPACT_COMMAND, "description": COMPACT_DESCRIPTION},
                {"name": SKILL, "description": SKILL_DESCRIPTION, "input": {"hint": SKILL_HINT}},
                {
                    "name": PROMPT,
                    "description": PROMPT_DESCRIPTION,
                    "input": {"hint": format!("<{REQUIRED_ARG}> [{OPTIONAL_ARG}]")},
                },
            ])
        );
    }

    #[test]
    fn an_empty_catalog_still_offers_compact() {
        let listed = serde_json::to_value(CommandCatalog::default().commands()).unwrap();
        let names: Vec<&Value> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|command| &command["name"])
            .collect();
        assert_eq!(names, [COMPACT_COMMAND]);
    }

    #[test_case(format!("/{COMPACT_COMMAND}"), Some(COMPACT_COMMAND.to_owned()) ; "compact")]
    #[test_case(format!("/{COMPACT_COMMAND} {ARGS}"), Some(COMPACT_COMMAND.to_owned()) ; "compact_ignores_arguments")]
    #[test_case(format!("/{SKILL} {ARGS}"), Some(format!("{SKILL_KIND} {SKILL}")) ; "skill_wins_over_prompt")]
    #[test_case(format!("/{PROMPT}  {ARGS} "), Some(format!("{PROMPT_KIND} {PROMPT} {ARGS}")) ; "mcp_prompt_with_args")]
    #[test_case(format!("/{UNKNOWN} {ARGS}"), None ; "unknown_command_is_plain_text")]
    #[test_case(COMPACT_COMMAND.to_owned(), None ; "no_slash")]
    #[test_case(format!("/ {COMPACT_COMMAND}"), None ; "empty_name")]
    #[test_case(format!(" /{COMPACT_COMMAND}"), None ; "slash_not_first")]
    fn prompts_route_to_the_command_they_name(text: String, expected: Option<String>) {
        let catalog = catalog();
        let routed = catalog.route(&text).map(|invocation| match invocation {
            Invocation::Compact => COMPACT_COMMAND.to_owned(),
            Invocation::Skill(name) => format!("{SKILL_KIND} {name}"),
            Invocation::McpPrompt { prompt, args } => {
                format!("{PROMPT_KIND} {} {args}", prompt.display_name)
            }
        });
        assert_eq!(routed, expected);
    }
}
