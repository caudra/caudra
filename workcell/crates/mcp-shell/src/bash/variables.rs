//! Builtins that change only the shell's own variables and options, read for what they leave the
//! commands after them on the same line. Their changes end with the line.

const UNSET: &str = "unset";
const SET: &str = "set";
const EXPORT: &str = "export";
const READONLY: &str = "readonly";
const DECLARATIONS: &[&str] = &[EXPORT, "declare", "typeset", READONLY, "local"];
const PRINT_FLAG: &str = "-p";
const UNSET_VARIABLE_FLAG: &str = "-v";
const END_OF_OPTIONS: &str = "--";
const OPTION_PREFIXES: [char; 2] = ['-', '+'];
const NAMED_OPTION_FLAG: char = 'o';
/// `set` flags that only decide whether a failure or an unset variable ends the line, and whether
/// commands are traced. Startup withholds `PS4`, so tracing expands nothing the line did not set.
const INERT_SET_FLAGS: &str = "eux";
const INERT_SET_OPTIONS: &[&str] = &["errexit", "nounset", "xtrace", "pipefail"];
const ASSIGNMENT: char = '=';
const APPEND: char = '+';

/// What a variable builtin leaves the commands after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VariableEffect<'a> {
    /// `unset` of variables the source names.
    Clears(Vec<&'a str>),
    /// `set` options that only decide when the line stops and whether it is traced.
    Options,
    /// Prints variables or options and changes nothing.
    Prints,
    /// Sets a variable, an attribute, a function, or an option, or clears something the source
    /// does not name.
    Changes,
}

impl VariableEffect<'_> {
    /// Whether it leaves every option and every variable in `read` as the line found them.
    pub fn keeps(&self, read: &[&str]) -> bool {
        match self {
            Self::Clears(names) => !names.iter().any(|name| read.contains(name)),
            Self::Options | Self::Prints => true,
            Self::Changes => false,
        }
    }
}

/// The effect of `argv` when it runs a variable builtin, or `None` when it does not.
pub fn variable_effect<'a>(argv: &[&'a str]) -> Option<VariableEffect<'a>> {
    let (&builtin, operands) = argv.split_first()?;
    Some(match builtin {
        UNSET => unset_effect(operands),
        SET => set_effect(operands),
        EXPORT | READONLY if operands.iter().all(|operand| *operand == PRINT_FLAG) => {
            VariableEffect::Prints
        }
        EXPORT | READONLY => VariableEffect::Changes,
        builtin if DECLARATIONS.contains(&builtin) => declaration_effect(operands),
        _ => return None,
    })
}

/// Whether `name` is a variable builtin, so that `variable_effect` reads it.
pub fn variable_builtin(name: &str) -> bool {
    name == SET || names_variables(name)
}

/// Whether the operands of `builtin` name variables.
pub fn names_variables(builtin: &str) -> bool {
    builtin == UNSET || DECLARATIONS.contains(&builtin)
}

/// The name an operand of `builtin` names: the whole operand for `unset`, and the part before its
/// assignment for a declaration, `A` in `A`, `A=1`, and `A+=1`.
pub fn variable_name<'a>(builtin: &str, operand: &'a str) -> &'a str {
    if builtin == UNSET {
        return operand;
    }
    operand.split_once(ASSIGNMENT).map_or(operand, |(name, _)| {
        name.strip_suffix(APPEND).unwrap_or(name)
    })
}

/// A name Bash reads as nothing but itself. Anything else, such as `a[$(id)]`, is read as a
/// subscript, which Bash evaluates as arithmetic and so runs any substitution inside.
pub fn plain_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn unset_effect<'a>(operands: &[&'a str]) -> VariableEffect<'a> {
    let mut operands = operands.iter().copied().peekable();
    while let Some(option) = operands.next_if(|operand| operand.starts_with('-')) {
        match option {
            END_OF_OPTIONS => break,
            UNSET_VARIABLE_FLAG => {}
            _ => return VariableEffect::Changes,
        }
    }
    let names: Vec<_> = operands.collect();
    if names.iter().all(|name| plain_identifier(name)) {
        VariableEffect::Clears(names)
    } else {
        VariableEffect::Changes
    }
}

fn set_effect(operands: &[&str]) -> VariableEffect<'static> {
    if matches!(operands, [] | ["-o" | "+o"]) {
        return VariableEffect::Prints;
    }
    let mut operands = operands.iter();
    while let Some(option) = operands.next() {
        let Some(cluster) = option.strip_prefix(OPTION_PREFIXES) else {
            return VariableEffect::Changes;
        };
        let (flags, named) = cluster
            .strip_suffix(NAMED_OPTION_FLAG)
            .map_or((cluster, false), |flags| (flags, true));
        if (flags.is_empty() && !named) || !flags.chars().all(|flag| INERT_SET_FLAGS.contains(flag))
        {
            return VariableEffect::Changes;
        }
        if named
            && !operands
                .next()
                .is_some_and(|name| INERT_SET_OPTIONS.contains(name))
        {
            return VariableEffect::Changes;
        }
    }
    VariableEffect::Options
}

/// `declare`, `typeset`, and `local` print with `-p`, of every variable or of the ones named, and
/// with no operands at all.
fn declaration_effect(operands: &[&str]) -> VariableEffect<'static> {
    let options = operands
        .iter()
        .take_while(|operand| operand.starts_with(OPTION_PREFIXES))
        .count();
    let (options, names) = operands.split_at(options);
    let prints = options.iter().all(|option| *option == PRINT_FLAG)
        && (names.is_empty() || !options.is_empty())
        && names.iter().all(|name| plain_identifier(name));
    if prints {
        VariableEffect::Prints
    } else {
        VariableEffect::Changes
    }
}

#[cfg(test)]
mod tests {
    use super::{
        VariableEffect, names_variables, plain_identifier, variable_builtin, variable_effect,
        variable_name,
    };

    fn effect(source: &str) -> Option<VariableEffect<'_>> {
        variable_effect(&source.split_whitespace().collect::<Vec<_>>())
    }

    #[test]
    fn unset_clears_only_the_variables_the_source_names() {
        for (source, names) in [
            ("unset", &[][..]),
            ("unset FOO", &["FOO"][..]),
            ("unset -v FOO _bar2", &["FOO", "_bar2"][..]),
            ("unset -- PATH", &["PATH"][..]),
        ] {
            assert_eq!(
                effect(source),
                Some(VariableEffect::Clears(names.to_vec())),
                "{source}"
            );
        }
        for source in [
            "unset -f f",
            "unset -n ref",
            "unset a[0]",
            "unset 1x",
            "unset FOO -f",
        ] {
            assert_eq!(effect(source), Some(VariableEffect::Changes), "{source}");
        }
    }

    #[test]
    fn set_is_inert_only_for_failure_and_tracing_options() {
        for source in [
            "set -e",
            "set -euo pipefail",
            "set +x",
            "set -o errexit -o nounset",
            "set +o pipefail",
            "set -ex -u",
        ] {
            assert_eq!(effect(source), Some(VariableEffect::Options), "{source}");
        }
        for source in ["set", "set -o", "set +o"] {
            assert_eq!(effect(source), Some(VariableEffect::Prints), "{source}");
        }
        for source in [
            "set -k",
            "set -a",
            "set -f",
            "set -P",
            "set -",
            "set --",
            "set -- a",
            "set -o posix",
            "set -eo",
            "set -oe pipefail",
            "set foo",
        ] {
            assert_eq!(effect(source), Some(VariableEffect::Changes), "{source}");
        }
    }

    #[test]
    fn a_declaration_prints_only_without_setting_anything() {
        for source in [
            "export",
            "export -p",
            "readonly -p",
            "declare",
            "declare -p",
            "declare -p HOME PATH",
            "typeset -p x",
            "local",
        ] {
            assert_eq!(effect(source), Some(VariableEffect::Prints), "{source}");
        }
        for source in [
            "export A=1",
            "export A",
            "export -p A",
            "export -f f",
            "readonly A",
            "declare A",
            "declare -x A",
            "declare -i n=1",
            "declare -p A=1",
            "declare -p a[0]",
            "local x=1",
            "local -r x",
            "typeset +x A",
        ] {
            assert_eq!(effect(source), Some(VariableEffect::Changes), "{source}");
        }
    }

    #[test]
    fn only_variable_builtins_have_an_effect() {
        for source in ["printenv", "env A=1", "/usr/bin/unset A"] {
            assert_eq!(effect(source), None, "{source}");
        }
    }

    #[test]
    fn keeping_reads_the_cleared_names_and_every_option() {
        assert!(VariableEffect::Clears(vec!["FOO"]).keeps(&["PATH"]));
        assert!(!VariableEffect::Clears(vec!["FOO", "PATH"]).keeps(&["PATH"]));
        assert!(VariableEffect::Options.keeps(&["PATH"]));
        assert!(VariableEffect::Prints.keeps(&["PATH"]));
        assert!(!VariableEffect::Changes.keeps(&[]));
    }

    #[test]
    fn a_declared_name_ends_before_its_assignment_and_an_unset_one_never_does() {
        for (operand, name) in [
            ("A", "A"),
            ("A=1", "A"),
            ("A+=x", "A"),
            ("a[1]=x", "a[1]"),
            ("=x", ""),
        ] {
            assert_eq!(variable_name("export", operand), name, "{operand}");
        }
        assert_eq!(variable_name("unset", "A=1"), "A=1");
    }

    #[test]
    fn set_is_a_variable_builtin_whose_operands_name_no_variables() {
        assert!(variable_builtin("set") && !names_variables("set"));
        for builtin in ["unset", "export", "declare", "typeset", "readonly", "local"] {
            assert!(
                variable_builtin(builtin) && names_variables(builtin),
                "{builtin}"
            );
        }
        assert!(!variable_builtin("env"));
    }

    #[test]
    fn a_plain_identifier_cannot_hold_a_subscript() {
        for name in ["A", "_", "_a1", "PATH"] {
            assert!(plain_identifier(name), "{name}");
        }
        for name in ["", "1a", "a[0]", "a[$(id)]", "a-b", "a.b", "é"] {
            assert!(!plain_identifier(name), "{name}");
        }
    }
}
