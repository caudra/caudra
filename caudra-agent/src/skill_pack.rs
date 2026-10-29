//! The vendored skill packs `[plugins.skill]` turns on, each off unless its key is true: one
//! builtin skill that serves the pack's pages, and a `/<pack>:<skill>` command for every skill the
//! user runs.

use caudra_config::SkillPacks;
use caudra_skillpack::{Library, Skill};

use crate::command::{ARGUMENTS_PLACEHOLDER, CommandScope, CommandSource, CustomCommand};
use crate::tools::native::skill::{
    BuiltinSkill, SkillServe, install_builtin_skill, remove_builtin_skill,
};

/// How one pack is served. A plain pack serves its index and pages as they are; pstack tailors
/// both to the session's model roles.
pub struct Pack {
    pub description: &'static str,
    pub library: fn() -> &'static Library,
    pub index: fn(&SkillServe) -> String,
    pub load: fn(&str, &SkillServe) -> Result<String, String>,
    /// Whether a skill gets a command, `false` for the leaves other skills read.
    pub commanded: fn(&Skill) -> bool,
}

pub static MATTPOCOCK: Pack = Pack {
    description: caudra_mattpocock::SKILL_DESCRIPTION,
    library: caudra_mattpocock::library,
    index: |_| caudra_mattpocock::index(),
    load: |address, _| {
        caudra_mattpocock::library()
            .load(address)
            .map(|loaded| loaded.text)
    },
    commanded: |_| true,
};

pub static EMIL: Pack = Pack {
    description: caudra_emil::SKILL_DESCRIPTION,
    library: caudra_emil::library,
    index: |_| caudra_emil::index(),
    load: |address, _| {
        caudra_emil::library()
            .load(address)
            .map(|loaded| loaded.text)
    },
    commanded: |_| true,
};

/// Applied at startup and on every settings reload: the skill registry is process-wide.
pub fn install_all(packs: &SkillPacks) {
    crate::pstack::install(packs.pstack);
    install(&MATTPOCOCK, packs.mattpocock);
    install(&EMIL, packs.emil);
}

/// The `/<pack>:` commands of every pack that is on.
pub fn all_commands(packs: &SkillPacks) -> Vec<CustomCommand> {
    let mut commands = crate::pstack::commands(packs.pstack);
    commands.extend(commands_of(&MATTPOCOCK, packs.mattpocock));
    commands.extend(commands_of(&EMIL, packs.emil));
    commands
}

pub fn install(pack: &'static Pack, on: bool) {
    let name = (pack.library)().name();
    match on {
        true => install_builtin_skill(skill(pack)),
        false => remove_builtin_skill(name),
    }
}

pub fn commands_of(pack: &Pack, on: bool) -> Vec<CustomCommand> {
    match on {
        true => skill_commands(pack),
        false => Vec::new(),
    }
}

fn skill(pack: &'static Pack) -> BuiltinSkill {
    BuiltinSkill {
        name: (pack.library)().name().to_owned(),
        description: pack.description.to_owned(),
        resolve: Box::new(|serve| ((pack.index)(serve), None)),
        pages: Some(Box::new(|address, serve| (pack.load)(address, serve))),
    }
}

fn skill_commands(pack: &Pack) -> Vec<CustomCommand> {
    let library = (pack.library)();
    let name = library.name();
    library
        .skills()
        .iter()
        .filter(|skill| (pack.commanded)(skill))
        .map(|skill| CustomCommand {
            name: skill.name.clone(),
            description: skill.summary().to_owned(),
            content: format!(
                "Load the `{name}/{}` skill with the `skill` tool and follow it.\n\n{ARGUMENTS_PLACEHOLDER}",
                skill.name
            ),
            scope: CommandScope::Builtin(name),
            accepts_args: true,
            argument_hint: skill.argument_hint.clone(),
            source: CommandSource::Builtin,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const FIX: &str = "the onboarding flow";

    #[test_case(&MATTPOCOCK, "grill-me", "/mattpocock:grill-me", "`mattpocock/grill-me`" ; "mattpocock")]
    #[test_case(&EMIL, "prototype", "/emil:prototype", "`emil/prototype`" ; "emil_user_only")]
    fn every_pack_skill_has_a_scoped_command(
        pack: &Pack,
        skill: &str,
        display_name: &str,
        loaded: &str,
    ) {
        let commands = commands_of(pack, true);
        assert_eq!(commands.len(), (pack.library)().skills().len());
        let command = commands
            .iter()
            .find(|command| command.name == skill)
            .unwrap();
        assert_eq!(command.display_name(), display_name);
        let sent = command.render(FIX);
        assert!(sent.contains(loaded) && sent.ends_with(FIX), "{sent}");
    }

    #[test_case(SkillPacks::default(), (false, false, false) ; "all_off")]
    #[test_case(SkillPacks { mattpocock: true, ..SkillPacks::default() }, (false, true, false) ; "mattpocock_alone")]
    #[test_case(SkillPacks { emil: true, ..SkillPacks::default() }, (false, false, true) ; "emil_alone")]
    #[test_case(SkillPacks { pstack: true, mattpocock: true, emil: true }, (true, true, true) ; "all_on")]
    fn each_pack_follows_its_own_switch(packs: SkillPacks, expected: (bool, bool, bool)) {
        let commands = all_commands(&packs);
        let has = |scope: &'static str| {
            commands
                .iter()
                .any(|command| command.scope == CommandScope::Builtin(scope))
        };
        let scopes = (
            has(caudra_pstack::NAME),
            has(caudra_mattpocock::NAME),
            has(caudra_emil::NAME),
        );
        assert_eq!(scopes, expected);
    }
}
