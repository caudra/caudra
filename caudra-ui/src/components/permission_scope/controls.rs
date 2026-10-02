use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, MAX_PATTERN_SLOTS, OptionLikePolicy, PatternDefinition,
    PatternSlot, PatternToken, SlotCombinations, SlotId,
};
use std::collections::BTreeSet;

pub(crate) const DOMAIN_COUNT: usize = 5;
const STRUCTURE_TARGET_MISSING: &str = "Select an existing argument or slot first.";
const STRUCTURE_ROLE_FIXED: &str =
    "This host-derived role is fixed. Reanalyze a concrete source before changing its structure.";
const STRUCTURE_SLOT_LIMIT: &str = "The template has reached its slot limit.";

pub(crate) enum TemplateStructureEdit {
    Add { token: usize },
    Remove { id: SlotId, literal: String },
    Link { token: usize, id: SlotId },
}

pub(crate) fn propose_structure(
    definition: &PatternDefinition,
    edit: TemplateStructureEdit,
) -> Result<PatternDefinition, &'static str> {
    let mut proposed = definition.clone();
    match edit {
        TemplateStructureEdit::Add { token } => {
            if proposed.slots.len() >= MAX_PATTERN_SLOTS {
                return Err(STRUCTURE_SLOT_LIMIT);
            }
            let original = proposed.argv.get(token).ok_or(STRUCTURE_TARGET_MISSING)?;
            let (role, domain, previous, fixed) = match original {
                PatternToken::Exact { value, role } => (
                    role.clone(),
                    ArgumentDomain::Exact {
                        value: value.clone(),
                    },
                    None,
                    Some(value.clone()),
                ),
                PatternToken::Slot { id, role } => {
                    let slot = proposed
                        .slots
                        .iter()
                        .find(|slot| slot.id == *id)
                        .ok_or(STRUCTURE_TARGET_MISSING)?;
                    (role.clone(), slot.domain.clone(), Some(*id), None)
                }
            };
            if !matches!(role, ArgumentRole::Data | ArgumentRole::Unknown) {
                return Err(STRUCTURE_ROLE_FIXED);
            }
            let next = proposed
                .slots
                .iter()
                .map(|slot| slot.id.0)
                .max()
                .unwrap_or_default()
                .checked_add(1)
                .ok_or(STRUCTURE_SLOT_LIMIT)?;
            let id = SlotId(next);
            proposed.argv[token] = PatternToken::Slot { id, role };
            proposed.slots.push(PatternSlot {
                id,
                label: format!("argument {next}"),
                domain,
                option_like: OptionLikePolicy::Reject,
            });
            if let SlotCombinations::ObservedTuples { tuples } = &mut proposed.combinations {
                *tuples = tuples
                    .iter()
                    .map(|tuple| {
                        let mut tuple = tuple.clone();
                        if let Some(value) = previous
                            .and_then(|id| tuple.get(&id).cloned())
                            .or_else(|| fixed.clone())
                        {
                            tuple.insert(id, value);
                        }
                        tuple
                    })
                    .collect();
            }
        }
        TemplateStructureEdit::Remove { id, literal } => {
            if !proposed.slots.iter().any(|slot| slot.id == id) {
                return Err(STRUCTURE_TARGET_MISSING);
            }
            for token in &mut proposed.argv {
                if let PatternToken::Slot { id: current, role } = token
                    && *current == id
                {
                    *token = PatternToken::Exact {
                        value: literal.clone(),
                        role: role.clone(),
                    };
                }
            }
        }
        TemplateStructureEdit::Link { token, id } => {
            if !proposed.slots.iter().any(|slot| slot.id == id) {
                return Err(STRUCTURE_TARGET_MISSING);
            }
            let role = match proposed.argv.get(token).ok_or(STRUCTURE_TARGET_MISSING)? {
                PatternToken::Exact { role, .. } | PatternToken::Slot { role, .. } => role.clone(),
            };
            if !matches!(role, ArgumentRole::Data | ArgumentRole::Unknown) {
                return Err(STRUCTURE_ROLE_FIXED);
            }
            proposed.argv[token] = PatternToken::Slot { id, role };
        }
    }
    let used: BTreeSet<_> = proposed
        .argv
        .iter()
        .filter_map(|token| {
            if let PatternToken::Slot { id, .. } = token {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    proposed.slots.retain(|slot| used.contains(&slot.id));
    if let SlotCombinations::ObservedTuples { tuples } = &mut proposed.combinations {
        *tuples = tuples
            .iter()
            .map(|tuple| {
                tuple
                    .iter()
                    .filter(|(id, _)| used.contains(id))
                    .map(|(id, value)| (*id, value.clone()))
                    .collect()
            })
            .collect();
    }
    Ok(proposed)
}

pub(crate) fn domain_index(domain: &ArgumentDomain) -> usize {
    match domain {
        ArgumentDomain::ObservedSet { .. } => 0,
        ArgumentDomain::Exact { .. } => 1,
        ArgumentDomain::Glob { .. } => 2,
        ArgumentDomain::Regex { .. } => 3,
        ArgumentDomain::AnyLiteralArgument => 4,
    }
}

pub(crate) fn domain_name(domain: &ArgumentDomain) -> &'static str {
    match domain {
        ArgumentDomain::ObservedSet { .. } => "Values seen before",
        ArgumentDomain::Exact { .. } => "Exact value",
        ArgumentDomain::Glob { .. } => "Wildcard",
        ArgumentDomain::Regex { .. } => "Regular expression",
        ArgumentDomain::AnyLiteralArgument => "Any argument",
    }
}

pub(crate) fn domain_for_mode(
    mode: usize,
    values: BTreeSet<String>,
    exact: Option<String>,
) -> ArgumentDomain {
    match mode {
        0 => ArgumentDomain::ObservedSet { values },
        1 => ArgumentDomain::Exact {
            value: exact
                .or_else(|| values.first().cloned())
                .unwrap_or_default(),
        },
        2 => ArgumentDomain::Glob {
            pattern: String::new(),
        },
        3 => ArgumentDomain::Regex {
            pattern: String::new(),
        },
        _ => ArgumentDomain::AnyLiteralArgument,
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::permission_patterns::{
        ArgumentRole, PatternToken, SlotCombinations, SlotId,
    };
    use test_case::test_case;

    use super::{STRUCTURE_ROLE_FIXED, TemplateStructureEdit, propose_structure};
    use crate::components::permission_scope::tests::template;

    const QUERY: SlotId = SlotId(1);
    const PATH: SlotId = SlotId(2);
    const FIXED_TOKEN: usize = 3;
    const QUERY_TOKEN: usize = 2;
    const PATH_TOKEN: usize = 5;
    const REPLACEMENT: &str = "fixed replacement";

    #[test_case(ArgumentRole::Executable; "executable")]
    #[test_case(ArgumentRole::Operation; "operation")]
    #[test_case(ArgumentRole::Flag; "flag")]
    #[test_case(ArgumentRole::OptionTerminator; "option_terminator")]
    #[test_case(ArgumentRole::Sensitive; "sensitive")]
    #[test_case(ArgumentRole::Payload; "payload")]
    fn fixed_roles_cannot_be_generalized_or_linked(role: ArgumentRole) {
        let mut definition = template();
        definition.argv[FIXED_TOKEN] = PatternToken::Exact {
            value: REPLACEMENT.into(),
            role,
        };
        for edit in [
            TemplateStructureEdit::Add { token: FIXED_TOKEN },
            TemplateStructureEdit::Link {
                token: FIXED_TOKEN,
                id: QUERY,
            },
        ] {
            assert_eq!(
                propose_structure(&definition, edit),
                Err(STRUCTURE_ROLE_FIXED)
            );
        }
    }

    #[test_case(FIXED_TOKEN; "new_slot_from_empty_literal")]
    #[test_case(QUERY_TOKEN; "unlink_repeated_slot")]
    fn adding_a_slot_retains_roles_and_listed_tuple_correlations(token: usize) {
        let definition = template();
        let proposed =
            propose_structure(&definition, TemplateStructureEdit::Add { token }).unwrap();
        let PatternToken::Slot { id, role } = &proposed.argv[token] else {
            panic!("expected proposed slot");
        };
        assert_eq!(role, &ArgumentRole::Data);
        assert_eq!(proposed.context, definition.context);
        assert_eq!(proposed.slots.len(), definition.slots.len() + 1);
        for (index, (before, after)) in definition.argv.iter().zip(&proposed.argv).enumerate() {
            if index != token {
                assert_eq!(before, after);
            }
        }
        let SlotCombinations::ObservedTuples { tuples } = &definition.combinations else {
            panic!("expected original listed tuples");
        };
        let expected = tuples
            .iter()
            .map(|tuple| {
                let mut tuple = tuple.clone();
                let value = if token == FIXED_TOKEN {
                    String::new()
                } else {
                    tuple[&QUERY].clone()
                };
                tuple.insert(*id, value);
                tuple
            })
            .collect();
        assert_eq!(
            proposed.combinations,
            SlotCombinations::ObservedTuples { tuples: expected }
        );
        if token == QUERY_TOKEN {
            assert_eq!(
                proposed.slots.last().unwrap().domain,
                definition.slots[0].domain
            );
        }
    }

    #[test_case(false; "remove_all_linked_occurrences")]
    #[test_case(true; "link_and_prune_unused_slot")]
    fn structural_proposals_prune_only_unused_slot_columns(link: bool) {
        let definition = template();
        let (edit, retained) = if link {
            (
                TemplateStructureEdit::Link {
                    token: PATH_TOKEN,
                    id: QUERY,
                },
                QUERY,
            )
        } else {
            (
                TemplateStructureEdit::Remove {
                    id: QUERY,
                    literal: REPLACEMENT.into(),
                },
                PATH,
            )
        };
        let proposed = propose_structure(&definition, edit).unwrap();
        assert_eq!(
            proposed
                .slots
                .iter()
                .map(|slot| slot.id)
                .collect::<Vec<_>>(),
            vec![retained]
        );
        for (before, after) in definition.argv.iter().zip(&proposed.argv) {
            if let PatternToken::Slot { id, role } = before {
                let expected = if !link && *id == QUERY {
                    PatternToken::Exact {
                        value: REPLACEMENT.into(),
                        role: role.clone(),
                    }
                } else {
                    PatternToken::Slot {
                        id: retained,
                        role: role.clone(),
                    }
                };
                assert_eq!(after, &expected);
            } else {
                assert_eq!(before, after);
            }
        }
        let SlotCombinations::ObservedTuples { tuples } = &definition.combinations else {
            panic!("expected original listed tuples");
        };
        let expected = tuples
            .iter()
            .map(|tuple| [(retained, tuple[&retained].clone())].into())
            .collect();
        assert_eq!(
            proposed.combinations,
            SlotCombinations::ObservedTuples { tuples: expected }
        );
    }
}
