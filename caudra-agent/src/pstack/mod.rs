//! The bundled pstack, off unless `[plugins.skill] pstack = true`: its `pstack` skill, the
//! `/pstack:<skill>` commands, and the `pstack-<agent>` task profiles.

use std::sync::atomic::{AtomicBool, Ordering};

use caudra_pstack::{Agent, Library, NAME, Roles, SKILL_DESCRIPTION, library};

use crate::command::{ARGUMENTS_PLACEHOLDER, CommandScope, CommandSource, CustomCommand};
use crate::prompt::profile::SystemPromptProfile;
use crate::tools::native::skill::{BuiltinSkill, install_builtin_skill, remove_builtin_skill};

const ADAPTER_ADDRESS: &str = "pstack/adapter";
/// Skills an agent's body sends it to read first, loaded by address instead.
const PRELOADS: &[(&str, &str)] = &[("poteto-agent", "pstack/poteto-mode")];

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Applied at startup and on every settings reload; while off, everything here is empty.
pub fn configure(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    match on {
        true => install_builtin_skill(skill()),
        false => remove_builtin_skill(NAME),
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn commands() -> Vec<CustomCommand> {
    match enabled() {
        true => builtin_commands(library()),
        false => Vec::new(),
    }
}

pub fn profiles() -> Vec<SystemPromptProfile> {
    match enabled() {
        true => library().agents().iter().map(profile).collect(),
        false => Vec::new(),
    }
}

fn skill() -> BuiltinSkill {
    BuiltinSkill {
        name: NAME.to_owned(),
        description: SKILL_DESCRIPTION.to_owned(),
        resolve: Box::new(|| (library().index(&roles()), None)),
        pages: Some(Box::new(|address| library().load(address))),
    }
}

fn roles() -> Roles {
    match caudra_storage::paths::config_dir() {
        Ok(dir) => Roles::read(&dir),
        Err(_) => Roles::Missing {
            path: caudra_pstack::ROLES_FILE.to_owned(),
        },
    }
}

/// One per skill a user runs; the `principle-*` leaves are read from within other skills.
fn builtin_commands(library: &Library) -> Vec<CustomCommand> {
    library
        .skills()
        .iter()
        .filter(|skill| !skill.is_principle())
        .map(|skill| CustomCommand {
            name: skill.name.clone(),
            description: skill.summary().to_owned(),
            content: format!(
                "Load the `{NAME}/{}` skill with the `skill` tool and follow it.\n\n{ARGUMENTS_PLACEHOLDER}",
                skill.name
            ),
            scope: CommandScope::Builtin(NAME),
            accepts_args: true,
            source: CommandSource::Builtin,
        })
        .collect()
}

fn profile(agent: &Agent) -> SystemPromptProfile {
    let name = format!("{NAME}-{}", agent.name);
    let address = format!("{NAME}/agents/{}.md", agent.name);
    let preload = PRELOADS
        .iter()
        .find(|(agent_name, _)| *agent_name == agent.name)
        .map(|(_, skill)| {
            format!(
                " Then load `{skill}` and read it in full: it is the skill the instructions \
                 below send you to find by path."
            )
        })
        .unwrap_or_default();
    let body = format!(
        "You run as pstack's `{agent}` agent. Before any work, load `{ADAPTER_ADDRESS}` with the \
         `skill` tool: the instructions below are written for Cursor, and the adapter maps their \
         tools, paths, and subagent fields to Caudra's.{preload}\n\n{body}",
        agent = agent.name,
        body = agent.body.trim(),
    );
    SystemPromptProfile::builtin(&name, &agent.description, body, &address)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const POTETO_MODE: &str = "poteto-mode";
    const PRINCIPLE: &str = "principle-";
    const POTETO_PROFILE: &str = "pstack-poteto-agent";
    const SICKO_PROFILE: &str = "pstack-comment-sicko";
    const FIX: &str = "fix the flaky test";

    #[test]
    fn every_user_facing_skill_has_a_command() {
        let commands = builtin_commands(library());
        let poteto = commands
            .iter()
            .find(|command| command.name == POTETO_MODE)
            .unwrap();
        assert_eq!(poteto.display_name(), "/pstack:poteto-mode");
        let sent = poteto.render(FIX);
        assert!(sent.contains("`pstack/poteto-mode`") && sent.ends_with(FIX), "{sent}");
        assert!(!commands.iter().any(|command| command.name.starts_with(PRINCIPLE)));
        let wanted = library()
            .skills()
            .iter()
            .filter(|skill| !skill.is_principle())
            .count();
        assert_eq!(commands.len(), wanted);
    }

    #[test_case(POTETO_PROFILE, true ; "poteto_agent_preloads_poteto_mode")]
    #[test_case(SICKO_PROFILE, false ; "comment_sicko_loads_the_adapter_only")]
    fn agents_become_profiles_that_load_the_adapter(name: &str, preloads: bool) {
        let profiles: Vec<SystemPromptProfile> = library().agents().iter().map(profile).collect();
        let found = profiles
            .iter()
            .find(|profile| profile.name() == name)
            .unwrap();
        assert!(found.body().contains(ADAPTER_ADDRESS));
        assert_eq!(found.body().contains("`pstack/poteto-mode`"), preloads);
    }

    #[test]
    fn everything_is_empty_while_off() {
        assert!(!enabled());
        assert!(commands().is_empty() && profiles().is_empty());
    }
}
