use caudra_storage::permission_patterns::{ArgumentRole, PatternContext, is_option_like};

use super::{CommandObservation, ObservationProvenance};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ObservationKey {
    source: String,
    observation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum IndexedArgument {
    Fixed { value: String, role: ArgumentRole },
    Variable { role: ArgumentRole },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ShapeKey {
    context: PatternContext,
    provenance: ObservationProvenance,
    argv: Vec<IndexedArgument>,
}

impl ObservationKey {
    pub(super) fn new(observation: &CommandObservation) -> Self {
        Self {
            source: observation.source.source_identity.clone(),
            observation: observation.source.observation_id.clone(),
        }
    }
}

impl ShapeKey {
    pub(super) fn new(observation: &CommandObservation) -> Self {
        let operation = observation
            .roles
            .iter()
            .position(|role| *role == ArgumentRole::Operation)
            .or_else(|| {
                observation
                    .argv
                    .get(1)
                    .filter(|value| !is_option_like(value))
                    .map(|_| 1)
            });
        let flag = observation
            .argv
            .iter()
            .zip(&observation.roles)
            .position(|(value, role)| *role == ArgumentRole::Flag && is_option_like(value));
        let argv = observation
            .argv
            .iter()
            .zip(&observation.roles)
            .enumerate()
            .map(|(index, (value, role))| {
                let variable = index > 1
                    && !is_option_like(value)
                    && (*role == ArgumentRole::Data
                        || (*role == ArgumentRole::Unknown
                            && (operation.is_some_and(|operation| index > operation)
                                || flag.is_some_and(|flag| index > flag))));
                if variable {
                    IndexedArgument::Variable { role: role.clone() }
                } else {
                    IndexedArgument::Fixed {
                        value: value.clone(),
                        role: role.clone(),
                    }
                }
            })
            .collect();
        Self {
            context: observation.context.clone(),
            provenance: observation.source.provenance.clone(),
            argv,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ObservationKey, ShapeKey};
    use crate::permissions::pattern_recognition::fixtures::observation;
    use caudra_storage::permission_patterns::ArgumentRole;
    use test_case::test_case;

    #[test_case("ab", "c", "a", "bc"; "identities_are_length_delimited")]
    fn identity_keys_do_not_conflate_concatenated_fields(
        left_source: &str,
        left_id: &str,
        right_source: &str,
        right_id: &str,
    ) {
        let left = ObservationKey {
            source: left_source.into(),
            observation: left_id.into(),
        };
        let right = ObservationKey {
            source: right_source.into(),
            observation: right_id.into(),
        };
        assert_ne!(left, right);
    }

    #[test_case(ArgumentRole::Flag, true; "fixed_flag_allows_finite_unknowns")]
    #[test_case(ArgumentRole::Unknown, false; "unclassified_flag_does_not_invent_arity")]
    #[test_case(ArgumentRole::Data, false; "option_looking_data_is_not_a_flag")]
    fn flag_first_discovery_requires_a_fixed_flag(role: ArgumentRole, same_shape: bool) {
        let mut first = observation(&["unlisted", "-n", "alpha", "left"], "first");
        first.roles[1] = role;
        let mut second = first.clone();
        second.argv[2] = "beta".into();
        second.argv[3] = "right".into();
        assert_eq!(ShapeKey::new(&first) == ShapeKey::new(&second), same_shape);
    }

    #[test_case(1, ArgumentRole::Unknown; "first_operand")]
    #[test_case(2, ArgumentRole::Operation; "operation_after_flag")]
    #[test_case(3, ArgumentRole::Operation; "later_operation")]
    fn fixed_authority_words_never_become_slots(index: usize, role: ArgumentRole) {
        let mut first = observation(&["unlisted", "--root", "inspect", "alpha"], "first");
        first.argv[index] = "inspect".into();
        first.roles[1] = ArgumentRole::Flag;
        first.roles[index] = role;
        let mut second = first.clone();
        second.argv[index] = "mutate".into();
        assert_ne!(ShapeKey::new(&first), ShapeKey::new(&second));
    }
}
