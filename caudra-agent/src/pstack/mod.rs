//! The bundled pstack, off unless `[plugins.skill] pstack = true`: its `pstack` skill, its
//! bundled automations, the `/pstack:<skill>` commands, and the `pstack-<agent>` task profiles.

use caudra_pstack::{Agent, Library, NAME, RoleTable, SKILL_DESCRIPTION, automations, library};

use crate::automation::bundled::{install_bundled_automations, remove_bundled_automations};
use crate::command::{ARGUMENTS_PLACEHOLDER, CommandScope, CommandSource, CustomCommand};
use crate::prompt::profile::SystemPromptProfile;
use crate::tools::native::skill::{
    BuiltinSkill, SkillServe, install_builtin_skill, remove_builtin_skill,
};

pub mod roles;

/// Applied at startup and on every settings reload. The skill and automation registries are
/// process-wide; the commands and profiles follow the config each caller holds.
pub fn install(on: bool) {
    match on {
        true => install_builtin_skill(skill()),
        false => remove_builtin_skill(NAME),
    }
    install_automations(on);
}

fn install_automations(on: bool) {
    match on {
        true => install_bundled_automations(NAME, automations()),
        false => remove_bundled_automations(NAME),
    }
}

pub fn commands(on: bool) -> Vec<CustomCommand> {
    match on {
        true => builtin_commands(library()),
        false => Vec::new(),
    }
}

pub fn profiles(on: bool) -> Vec<SystemPromptProfile> {
    match on {
        true => library().agents().iter().map(profile).collect(),
        false => Vec::new(),
    }
}

fn skill() -> BuiltinSkill {
    BuiltinSkill {
        name: NAME.to_owned(),
        description: SKILL_DESCRIPTION.to_owned(),
        resolve: Box::new(|serve| (library().index(&table(serve)), None)),
        pages: Some(Box::new(|address, serve| {
            library().load(address, &table(serve))
        })),
    }
}

fn table(serve: &SkillServe) -> RoleTable {
    roles::table(&serve.model_policy, serve.task_model_choice)
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
    SystemPromptProfile::builtin(
        &name,
        &agent.description,
        agent.body.trim().to_owned(),
        &address,
    )
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use caudra_config::FeatureFlags;

    use crate::automation::bundled::{BundledAutomation, bundled_automations};
    use crate::automation::catalog::{Catalog, Frontend};

    const SHIPPED_AUTOMATIONS: [&str; 3] = ["benny-reproduce", "benny-triage", "pstack-watch-pr"];
    const POTETO_MODE: &str = "poteto-mode";
    const PRINCIPLE: &str = "principle-";
    const POTETO_PROFILE: &str = "pstack-poteto-agent";
    const SICKO_PROFILE: &str = "pstack-comment-sicko";
    const FIX: &str = "fix the flaky test";
    const FOLLOW_IT: &str = "and follow it.";

    #[test]
    fn every_user_facing_skill_has_a_command() {
        let commands = builtin_commands(library());
        let poteto = commands
            .iter()
            .find(|command| command.name == POTETO_MODE)
            .unwrap();
        assert_eq!(poteto.display_name(), "/pstack:poteto-mode");
        let sent = poteto.render(FIX);
        assert!(
            sent.contains("`pstack/poteto-mode`") && sent.ends_with(FIX),
            "{sent}"
        );
        assert!(
            !commands
                .iter()
                .any(|command| command.name.starts_with(PRINCIPLE))
        );
        let wanted = library()
            .skills()
            .iter()
            .filter(|skill| !skill.is_principle())
            .count();
        assert_eq!(commands.len(), wanted);
    }

    #[test_case(POTETO_PROFILE, true ; "poteto_agent_reads_poteto_mode")]
    #[test_case(SICKO_PROFILE, false ; "comment_sicko_reads_its_own_text")]
    fn agents_become_profiles(name: &str, reads_poteto_mode: bool) {
        let profiles: Vec<SystemPromptProfile> = library().agents().iter().map(profile).collect();
        let found = profiles
            .iter()
            .find(|profile| profile.name() == name)
            .unwrap();
        assert_eq!(
            found.body().contains("`pstack/poteto-mode`"),
            reads_poteto_mode
        );
    }

    #[test_case(false ; "off")]
    #[test_case(true ; "on")]
    fn commands_and_profiles_follow_the_switch(on: bool) {
        let poteto_command = commands(on)
            .iter()
            .any(|command| command.name == POTETO_MODE);
        let poteto_profile = profiles(on)
            .iter()
            .any(|profile| profile.name() == POTETO_PROFILE);
        assert_eq!((poteto_command, poteto_profile), (on, on));
    }

    #[test]
    fn the_switch_installs_exactly_the_shipped_automations() {
        let installed = || -> Vec<BundledAutomation> {
            bundled_automations()
                .iter()
                .filter(|script| script.provider == NAME)
                .cloned()
                .collect()
        };

        install_automations(true);
        let shipped = installed();
        let names: Vec<&str> = shipped.iter().map(|script| script.name.as_str()).collect();
        assert_eq!(names, SHIPPED_AUTOMATIONS);
        let listed = Catalog::scan_user_only(None, &shipped, FeatureFlags::all(), Frontend::Tui)
            .to_catalog();
        assert!(listed.invalid.is_empty(), "{:?}", listed.invalid);
        assert_eq!(listed.entries.len(), SHIPPED_AUTOMATIONS.len());

        install_automations(false);
        assert!(installed().is_empty());
    }

    #[test]
    fn a_command_without_arguments_ends_on_its_instruction() {
        let commands = commands(true);
        let poteto = commands
            .iter()
            .find(|command| command.name == POTETO_MODE)
            .unwrap();
        assert!(
            poteto.render("").ends_with(FOLLOW_IT),
            "{:?}",
            poteto.render("")
        );
    }
}
