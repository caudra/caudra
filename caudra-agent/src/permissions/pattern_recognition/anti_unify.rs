use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, ObservedTuple, OptionLikePolicy, PATTERN_SCHEMA_VERSION,
    PatternDefinition, PatternSlot, PatternToken, PatternValidationError, SlotCombinations, SlotId,
};
use std::collections::BTreeSet;

use super::CommandObservation;

const CANDIDATE_NAME: &str = "Observed command pattern";
const LONG_FLAG_PREFIX: &str = "--";
const MAX_FLAG_NAME_BYTES: usize = 24;
const PATH_SEPARATOR: char = '/';
const PATH_SLOT: &str = "path";
const VALUE_SLOT: &str = "value";

pub(super) fn anti_unify(
    rows: &[&CommandObservation],
) -> Result<PatternDefinition, PatternValidationError> {
    let first = rows
        .first()
        .ok_or(PatternValidationError::InvalidExecutable)?;
    let mut argv = Vec::with_capacity(first.argv.len());
    let mut slots = Vec::new();
    let mut names = Vec::new();
    for (index, (value, role)) in first.argv.iter().zip(&first.roles).enumerate() {
        let values = rows
            .iter()
            .map(|row| {
                row.argv
                    .get(index)
                    .cloned()
                    .ok_or(PatternValidationError::InvalidArgument(index))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if values.len() > 1 {
            let id = SlotId(
                u16::try_from(index).map_err(|_| PatternValidationError::InvalidArgument(index))?,
            );
            names.push(slot_name(argv.last(), &values));
            slots.push(PatternSlot {
                id,
                label: String::new(),
                domain: ArgumentDomain::ObservedSet { values },
                option_like: OptionLikePolicy::Reject,
            });
            argv.push(PatternToken::Slot {
                id,
                role: role.clone(),
            });
        } else {
            argv.push(PatternToken::Exact {
                value: value.clone(),
                role: role.clone(),
            });
        }
    }
    for (position, slot) in slots.iter_mut().enumerate() {
        let name = &names[position];
        let same = |other: &&String| *other == name;
        slot.label = if names.iter().filter(same).count() > 1 {
            format!("<{name}{}>", names[..=position].iter().filter(same).count())
        } else {
            format!("<{name}>")
        };
    }
    let combinations = if slots.is_empty() {
        SlotCombinations::Independent
    } else {
        let tuples = rows
            .iter()
            .map(|row| {
                slots
                    .iter()
                    .map(|slot| (slot.id, row.argv[usize::from(slot.id.0)].clone()))
                    .collect::<ObservedTuple>()
            })
            .collect();
        SlotCombinations::ObservedTuples { tuples }
    };
    let definition = PatternDefinition {
        version: PATTERN_SCHEMA_VERSION,
        name: CANDIDATE_NAME.into(),
        context: first.context.clone(),
        argv,
        slots,
        combinations,
    };
    definition.validate()?;
    Ok(definition)
}

/// What a varying argument is called: the long flag it follows, as
/// `--package <package>`, a path when every value contains a slash, else a
/// value. Only a hint, as matching never reads it.
fn slot_name(previous: Option<&PatternToken>, values: &BTreeSet<String>) -> String {
    let flag = previous.and_then(|token| match token {
        PatternToken::Exact {
            value,
            role: ArgumentRole::Flag,
        } => value.strip_prefix(LONG_FLAG_PREFIX),
        _ => None,
    });
    match flag {
        Some(name)
            if name.len() <= MAX_FLAG_NAME_BYTES
                && name.starts_with(|character: char| character.is_ascii_alphanumeric())
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-') =>
        {
            name.into()
        }
        _ if values.iter().all(|value| value.contains(PATH_SEPARATOR)) => PATH_SLOT.into(),
        _ => VALUE_SLOT.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::anti_unify;
    use crate::permissions::pattern_recognition::fixtures::observation;
    use caudra_storage::permission_patterns::ArgumentRole;
    use test_case::test_case;

    const OPTION_TERMINATOR: &str = "--";

    /// The roles Workcell's analysis gives these words: leading words are
    /// operations until the first flag, and nothing after `--` is a flag.
    fn roles(argv: &[&str]) -> Vec<ArgumentRole> {
        let mut leading = true;
        let mut terminated = false;
        argv.iter()
            .enumerate()
            .map(|(index, value)| {
                if index == 0 {
                    ArgumentRole::Executable
                } else if terminated {
                    ArgumentRole::Unknown
                } else if *value == OPTION_TERMINATOR {
                    leading = false;
                    terminated = true;
                    ArgumentRole::OptionTerminator
                } else if value.starts_with('-') {
                    leading = false;
                    ArgumentRole::Flag
                } else if leading {
                    ArgumentRole::Operation
                } else {
                    ArgumentRole::Unknown
                }
            })
            .collect()
    }

    #[test_case(&["cargo", "test", "-p", "alpha"], &["cargo", "test", "-p", "beta"], &["<value>"]; "short_flag_value")]
    #[test_case(&["cargo", "build", "--package", "alpha"], &["cargo", "build", "--package", "beta"], &["<package>"]; "long_flag_names_its_value")]
    #[test_case(&["rg", "-n", "needle", "src/a"], &["rg", "-n", "other", "tests/b"], &["<value>", "<path>"]; "search_term_and_path")]
    #[test_case(&["cp", "-r", "a/x", "b/x"], &["cp", "-r", "c/y", "d/y"], &["<path1>", "<path2>"]; "repeated_names_are_numbered")]
    #[test_case(&["tool", "-x", "alpha", "a/x", "beta"], &["tool", "-x", "gamma", "c/y", "delta"], &["<value1>", "<path>", "<value2>"]; "only_repeated_names_are_numbered")]
    #[test_case(&["tool", "-x", "--x=1", "alpha"], &["tool", "-x", "--x=1", "beta"], &["<value>"]; "flag_with_inline_value")]
    #[test_case(&["tool", "-x", "--a-very-long-flag-name-past-the-cap", "alpha"], &["tool", "-x", "--a-very-long-flag-name-past-the-cap", "beta"], &["<value>"]; "long_flag_past_the_cap")]
    #[test_case(&["tool", "-x", "--", "alpha"], &["tool", "-x", "--", "beta"], &["<value>"]; "option_terminator")]
    #[test_case(&["tool", "-x", "--", "--name", "alpha"], &["tool", "-x", "--", "--name", "beta"], &["<value>"]; "flag_shaped_data")]
    fn slots_are_named_by_what_surrounds_them(first: &[&str], second: &[&str], expected: &[&str]) {
        let rows = [first, second].map(|argv| {
            let mut row = observation(argv, argv[argv.len() - 1]);
            row.roles = roles(argv);
            row
        });
        let labels: Vec<_> = anti_unify(&rows.iter().collect::<Vec<_>>())
            .unwrap()
            .slots
            .into_iter()
            .map(|slot| slot.label)
            .collect();
        assert_eq!(labels, expected);
    }
}
