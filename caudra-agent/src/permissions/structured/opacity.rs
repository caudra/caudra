use std::fmt;
use std::str::FromStr;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::{PermissionResource, PermissionResourceKind};

/// Set by the shell tool on the resource that stands for a whole command line
/// it could not review command by command.
pub const OPACITY_ATTRIBUTE: &str = "opacity";
const INLINE_SCRIPT: &str = "inline_script";
const LANGUAGE_SEPARATOR: char = ':';

/// Why a command line could not be reviewed command by command.
///
/// The declaration order is the severity, so a line's cause is the `max` of
/// its parts. `Unparsed`, `Privilege`, and `Indirect` always ask; the rest can
/// be screened by a decision engine that reads the full text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShellOpacity {
    ControlFlow,
    Redirect,
    Dynamic,
    Wrapper,
    InlineScript { language: ScriptLanguage },
    Indirect,
    Privilege,
    Unparsed,
}

/// The language of code handed to an interpreter inline, named from a fixed
/// table of interpreters and never from the command's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScriptLanguage {
    Shell,
    Python,
    JavaScript,
    Ruby,
    Perl,
    Php,
    Lua,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unrecognized shell opacity")]
pub struct UnknownShellOpacity;

impl ShellOpacity {
    const WITHOUT_LANGUAGE: [Self; 7] = [
        Self::ControlFlow,
        Self::Redirect,
        Self::Dynamic,
        Self::Wrapper,
        Self::Indirect,
        Self::Privilege,
        Self::Unparsed,
    ];

    /// The cause the shell tool recorded on a command resource, if any.
    pub fn of(resource: &PermissionResource) -> Option<Self> {
        if resource.kind != PermissionResourceKind::Command {
            return None;
        }
        resource.attributes.get(OPACITY_ATTRIBUTE)?.parse().ok()
    }

    /// Whether a decision engine may screen the line instead of it always
    /// asking.
    pub fn screenable(self) -> bool {
        !matches!(self, Self::Unparsed | Self::Privilege | Self::Indirect)
    }

    /// What the line does that no rule for its commands can check, as a prompt
    /// says it.
    pub fn caution(self) -> &'static str {
        match self {
            Self::ControlFlow => "Uses loops or conditions that rules can't check",
            Self::Redirect => "Redirects input or output in ways rules can't check",
            Self::Dynamic => "Builds part of the command only when it runs",
            Self::Wrapper => "Runs a command through another program",
            Self::InlineScript { language } => match language {
                ScriptLanguage::Shell => "Runs inline shell code that Caudra can't check",
                ScriptLanguage::Python => "Runs inline Python that Caudra can't check",
                ScriptLanguage::JavaScript => "Runs inline JavaScript that Caudra can't check",
                ScriptLanguage::Ruby => "Runs inline Ruby that Caudra can't check",
                ScriptLanguage::Perl => "Runs inline Perl that Caudra can't check",
                ScriptLanguage::Php => "Runs inline PHP that Caudra can't check",
                ScriptLanguage::Lua => "Runs inline Lua that Caudra can't check",
            },
            Self::Indirect => "Runs code through eval or source",
            Self::Privilege => "Runs with elevated privileges",
            Self::Unparsed => "Caudra couldn't read this command line",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ControlFlow => "control_flow",
            Self::Redirect => "redirect",
            Self::Dynamic => "dynamic",
            Self::Wrapper => "wrapper",
            Self::InlineScript { .. } => INLINE_SCRIPT,
            Self::Indirect => "indirect",
            Self::Privilege => "privilege",
            Self::Unparsed => "unparsed",
        }
    }
}

impl ScriptLanguage {
    const ALL: [Self; 7] = [
        Self::Shell,
        Self::Python,
        Self::JavaScript,
        Self::Ruby,
        Self::Perl,
        Self::Php,
        Self::Lua,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::Ruby => "ruby",
            Self::Perl => "perl",
            Self::Php => "php",
            Self::Lua => "lua",
        }
    }
}

impl fmt::Display for ShellOpacity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())?;
        if let Self::InlineScript { language } = self {
            write!(formatter, "{LANGUAGE_SEPARATOR}{}", language.name())?;
        }
        Ok(())
    }
}

impl FromStr for ShellOpacity {
    type Err = UnknownShellOpacity;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.split_once(LANGUAGE_SEPARATOR) {
            Some((INLINE_SCRIPT, name)) => ScriptLanguage::ALL
                .into_iter()
                .find(|language| language.name() == name)
                .map(|language| Self::InlineScript { language }),
            Some(_) => None,
            None => Self::WITHOUT_LANGUAGE
                .into_iter()
                .find(|cause| cause.name() == value),
        }
        .ok_or(UnknownShellOpacity)
    }
}

impl Serialize for ShellOpacity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ShellOpacity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{OPACITY_ATTRIBUTE, ScriptLanguage, ShellOpacity, UnknownShellOpacity};
    use crate::permissions::structured::tests::{custom_resource, protected_command_resource};
    use crate::permissions::structured::{
        PermissionResource, resource_constraint, resource_constraint_matches,
    };

    const WHOLE_LINE: &str = "git status > status.txt";
    const WORKDIR: &str = "/project";
    const CAUSES: &[ShellOpacity] = &[
        ShellOpacity::ControlFlow,
        ShellOpacity::Redirect,
        ShellOpacity::Dynamic,
        ShellOpacity::Wrapper,
        ShellOpacity::InlineScript {
            language: ScriptLanguage::Python,
        },
        ShellOpacity::Indirect,
        ShellOpacity::Privilege,
        ShellOpacity::Unparsed,
    ];

    fn opaque_line(opacity: &str) -> PermissionResource {
        let mut resource = protected_command_resource(WHOLE_LINE, WORKDIR);
        resource
            .attributes
            .insert(OPACITY_ATTRIBUTE.into(), opacity.into());
        resource
    }

    #[test_case(ShellOpacity::ControlFlow, "control_flow")]
    #[test_case(ShellOpacity::Redirect, "redirect")]
    #[test_case(ShellOpacity::Dynamic, "dynamic")]
    #[test_case(ShellOpacity::Wrapper, "wrapper")]
    #[test_case(ShellOpacity::Indirect, "indirect")]
    #[test_case(ShellOpacity::Privilege, "privilege")]
    #[test_case(ShellOpacity::Unparsed, "unparsed")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Shell }, "inline_script:shell")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Python }, "inline_script:python")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::JavaScript }, "inline_script:javascript")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Ruby }, "inline_script:ruby")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Perl }, "inline_script:perl")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Php }, "inline_script:php")]
    #[test_case(ShellOpacity::InlineScript { language: ScriptLanguage::Lua }, "inline_script:lua")]
    fn opacity_encoding_round_trips(cause: ShellOpacity, encoded: &str) {
        assert_eq!(cause.to_string(), encoded);
        assert_eq!(encoded.parse(), Ok(cause));
    }

    #[test_case("inline_script"; "a_script_without_its_language")]
    #[test_case("inline_script:cobol"; "a_language_outside_the_table")]
    #[test_case("unparsed:python"; "a_language_on_another_cause")]
    #[test_case("Unparsed"; "another_spelling")]
    #[test_case(""; "nothing")]
    fn unknown_encodings_are_refused(encoded: &str) {
        assert_eq!(encoded.parse::<ShellOpacity>(), Err(UnknownShellOpacity));
    }

    /// A line is as opaque as its worst part, so one cause that always asks has
    /// to outrank every cause an engine may screen, whatever else is on the line.
    #[test]
    fn every_cause_that_always_asks_outranks_every_screenable_one() {
        for always in CAUSES.iter().filter(|cause| !cause.screenable()) {
            for screenable in CAUSES.iter().filter(|cause| cause.screenable()) {
                assert!(always > screenable, "{always} must outrank {screenable}");
            }
        }
        assert_eq!(
            CAUSES
                .iter()
                .filter(|cause| !cause.screenable())
                .collect::<Vec<_>>(),
            [
                &ShellOpacity::Indirect,
                &ShellOpacity::Privilege,
                &ShellOpacity::Unparsed
            ]
        );
    }

    #[test_case(opaque_line("privilege"), Some(ShellOpacity::Privilege); "a_whole_line_command")]
    #[test_case(opaque_line("sudo"), None; "an_unrecognized_cause")]
    #[test_case(protected_command_resource(WHOLE_LINE, WORKDIR), None; "a_command_without_the_attribute")]
    #[test_case(
        PermissionResource { attributes: opaque_line("privilege").attributes, ..custom_resource(WHOLE_LINE) },
        None;
        "a_resource_that_is_not_a_command"
    )]
    fn opacity_is_read_only_from_command_resources(
        resource: PermissionResource,
        expected: Option<ShellOpacity>,
    ) {
        assert_eq!(ShellOpacity::of(&resource), expected);
    }

    /// The attribute is derived from the command and its workdir, which an exact
    /// rule already pins. Pinning it too would strand every rule stored before
    /// it existed and every rule stored before a cause is reclassified.
    #[test]
    fn exact_rules_neither_pin_nor_require_the_opacity() {
        let earlier = resource_constraint(&protected_command_resource(WHOLE_LINE, WORKDIR));
        let resource = opaque_line("redirect");

        assert!(resource_constraint_matches(&earlier, &resource));
        assert_eq!(resource_constraint(&resource), earlier);
    }
}
