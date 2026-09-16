use caudra_storage::permission_patterns::{
    ArgumentDomain, ObservedTuple, OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternDefinition,
    PatternSlot, PatternToken, PatternValidationError, SlotCombinations, SlotId,
};
use std::collections::BTreeSet;

use super::CommandObservation;

const CANDIDATE_NAME: &str = "Observed command pattern";

pub(super) fn anti_unify(
    rows: &[&CommandObservation],
) -> Result<PatternDefinition, PatternValidationError> {
    let first = rows
        .first()
        .ok_or(PatternValidationError::InvalidExecutable)?;
    let mut argv = Vec::with_capacity(first.argv.len());
    let mut slots = Vec::new();
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
            slots.push(PatternSlot {
                id,
                label: format!("<pattern{}>", slots.len() + 1),
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
