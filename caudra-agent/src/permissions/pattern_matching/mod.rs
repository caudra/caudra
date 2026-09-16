mod slots;

use caudra_storage::permission_patterns::{
    ArgumentRole, ObservedTuple, OptionLikePolicy, PatternDefinition, PatternToken,
    PatternValidationError, SlotCombinations, SlotId, is_option_like,
};
use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

use super::pattern_recognition::{CommandObservation, ObservationError};
use slots::CompiledExpression;
pub use slots::{
    GLOB_ARGUMENT_SEMANTICS, MAX_GLOB_REGEX_BYTES, REGEX_DFA_SIZE_LIMIT, REGEX_NEST_LIMIT,
    REGEX_SIZE_LIMIT,
};

pub struct CompiledPattern {
    definition: PatternDefinition,
    expressions: BTreeMap<SlotId, CompiledExpression>,
    fingerprint: String,
}

#[derive(Debug, Error)]
pub enum PatternCompileError {
    #[error(transparent)]
    Definition(#[from] PatternValidationError),
    #[error("invalid expression for slot {slot:?}: {reason}")]
    Expression { slot: SlotId, reason: String },
    #[error("observed tuple is outside the approved domain/option guard for slot {0:?}")]
    InvalidTuple(SlotId),
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatternMismatch {
    #[error("execution context differs: {field}")]
    Context { field: &'static str },
    #[error("argv length differs: expected {expected}, received {actual}")]
    ArgumentCount { expected: usize, actual: usize },
    #[error(
        "argument role differs at argv index {index}: expected {expected:?}, received {actual:?}"
    )]
    Role {
        index: usize,
        expected: ArgumentRole,
        actual: ArgumentRole,
    },
    #[error("exact argument differs at argv index {index}")]
    Literal { index: usize },
    #[error("option-looking value is not authorized for slot {slot:?} at argv index {index}")]
    OptionLike { index: usize, slot: SlotId },
    #[error("argument is outside slot {slot:?}'s domain at argv index {index}")]
    Domain { index: usize, slot: SlotId },
    #[error("repeated slot {slot:?} differs at argv index {index}")]
    Equality { index: usize, slot: SlotId },
    #[error("this joint tuple was not approved; independent combinations are disabled")]
    UnobservedTuple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchReport {
    Matched { bindings: ObservedTuple },
    NotMatched { reason: PatternMismatch },
}

impl MatchReport {
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Matched { .. })
    }
}

impl CompiledPattern {
    pub fn compile(definition: &PatternDefinition) -> Result<Self, PatternCompileError> {
        let fingerprint = definition.fingerprint()?;
        let expressions = definition
            .slots
            .iter()
            .map(|slot| CompiledExpression::compile(slot).map(|expression| (slot.id, expression)))
            .collect::<Result<_, _>>()?;
        let compiled = Self {
            definition: definition.clone(),
            expressions,
            fingerprint,
        };
        if let SlotCombinations::ObservedTuples { tuples } = &definition.combinations {
            for tuple in tuples {
                for (id, value) in tuple {
                    for (index, token) in definition.argv.iter().enumerate() {
                        if let PatternToken::Slot { id: token_id, role } = token
                            && token_id == id
                            && compiled.match_slot(*id, role, value, index).is_err()
                        {
                            return Err(PatternCompileError::InvalidTuple(*id));
                        }
                    }
                }
            }
        }
        Ok(compiled)
    }

    pub fn definition(&self) -> &PatternDefinition {
        &self.definition
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn matches(
        &self,
        observation: &CommandObservation,
    ) -> Result<MatchReport, ObservationError> {
        observation.validate()?;
        Ok(match self.match_verified(observation) {
            Ok(bindings) => MatchReport::Matched { bindings },
            Err(reason) => MatchReport::NotMatched { reason },
        })
    }

    fn match_verified(
        &self,
        observation: &CommandObservation,
    ) -> Result<ObservedTuple, PatternMismatch> {
        for ((field, expected), (_, actual)) in self
            .definition
            .context
            .fields()
            .into_iter()
            .zip(observation.context.fields())
        {
            if expected != actual {
                return Err(PatternMismatch::Context { field });
            }
        }
        if self.definition.argv.len() != observation.argv.len() {
            return Err(PatternMismatch::ArgumentCount {
                expected: self.definition.argv.len(),
                actual: observation.argv.len(),
            });
        }
        let mut bindings = BTreeMap::new();
        for (index, (token, value)) in self
            .definition
            .argv
            .iter()
            .zip(&observation.argv)
            .enumerate()
        {
            let (PatternToken::Exact { role, .. } | PatternToken::Slot { role, .. }) = token;
            if role != &observation.roles[index] {
                return Err(PatternMismatch::Role {
                    index,
                    expected: role.clone(),
                    actual: observation.roles[index].clone(),
                });
            }
            match token {
                PatternToken::Exact { value: exact, .. } if exact != value => {
                    return Err(PatternMismatch::Literal { index });
                }
                PatternToken::Slot { id, .. } => {
                    self.match_slot(*id, role, value, index)?;
                    if let Some(previous) = bindings.insert(*id, value.clone())
                        && previous != *value
                    {
                        return Err(PatternMismatch::Equality { index, slot: *id });
                    }
                }
                PatternToken::Exact { .. } => {}
            }
        }
        if let SlotCombinations::ObservedTuples { tuples } = &self.definition.combinations
            && !tuples.contains(&bindings)
        {
            return Err(PatternMismatch::UnobservedTuple);
        }
        Ok(bindings)
    }

    fn match_slot(
        &self,
        id: SlotId,
        role: &ArgumentRole,
        value: &str,
        index: usize,
    ) -> Result<(), PatternMismatch> {
        let Some(slot) = self.definition.slots.iter().find(|slot| slot.id == id) else {
            return Err(PatternMismatch::Domain { index, slot: id });
        };
        if is_option_like(value)
            && (slot.option_like != OptionLikePolicy::AllowForProvenData
                || *role != ArgumentRole::Data)
        {
            return Err(PatternMismatch::OptionLike { index, slot: id });
        }
        if !self
            .expressions
            .get(&id)
            .is_some_and(|expression| expression.matches(&slot.domain, value))
        {
            return Err(PatternMismatch::Domain { index, slot: id });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompiledPattern, MatchReport, PatternCompileError, PatternMismatch, REGEX_NEST_LIMIT,
    };
    use crate::permissions::pattern_recognition::{ShellEffectStatus, fixtures::observation};
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, MAX_ARGUMENT_BYTES, MAX_SLOT_EXPRESSION_BYTES,
        OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternDefinition, PatternSlot, PatternToken,
        SlotCombinations, SlotId,
    };
    use test_case::test_case;

    const ARGV: [&str; 3] = ["novelctl", "inspect", "alpha"];
    const SLOT: SlotId = SlotId(2);

    fn pattern(domain: ArgumentDomain) -> PatternDefinition {
        let row = observation(&ARGV, "fixture");
        PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: "Observed command pattern".into(),
            context: row.context,
            argv: vec![
                PatternToken::Exact {
                    value: ARGV[0].into(),
                    role: ArgumentRole::Executable,
                },
                PatternToken::Exact {
                    value: ARGV[1].into(),
                    role: ArgumentRole::Unknown,
                },
                PatternToken::Slot {
                    id: SLOT,
                    role: ArgumentRole::Unknown,
                },
            ],
            slots: vec![PatternSlot {
                id: SLOT,
                label: "<pattern1>".into(),
                domain,
                option_like: OptionLikePolicy::Reject,
            }],
            combinations: SlotCombinations::Independent,
        }
    }

    #[test_case("a|bc", "a", true; "first_alternative")]
    #[test_case("a|bc", "bc", true; "second_alternative")]
    #[test_case("a|bc", "xa", false; "no_prefix")]
    #[test_case("a|bc", "bcx", false; "no_suffix")]
    #[test_case("a|bc", "a\n", false; "no_trailing_newline")]
    #[test_case("(?m)^alpha$", "\nalpha\n", false; "multiline_cannot_escape_boundaries")]
    #[test_case("[αβ]+", "αβ", true; "unicode_scalars")]
    #[test_case("", "", true; "empty_literal")]
    #[test_case("", "alpha", false; "empty_is_not_any")]
    #[test_case("(?s).*", "name;echo text\n", true; "literal_operators_remain_data")]
    fn regex_is_whole_value(expression: &str, value: &str, expected: bool) {
        let compiled = CompiledPattern::compile(&pattern(ArgumentDomain::Regex {
            pattern: expression.into(),
        }))
        .unwrap();
        let row = observation(&[ARGV[0], ARGV[1], value], "matching");
        assert_eq!(compiled.matches(&row).unwrap().is_match(), expected);
    }

    #[test_case(r"(a)\1"; "no_backreferences")]
    #[test_case("(?=a)a"; "no_lookaround")]
    #[test_case("a)|.*(?:b"; "cannot_break_anchor_wrapper")]
    #[test_case("[a-z]{1000000}"; "compiled_size_limit")]
    fn invalid_regex_is_not_a_fallback(expression: &str) {
        assert!(matches!(
            CompiledPattern::compile(&pattern(ArgumentDomain::Regex {
                pattern: expression.into()
            })),
            Err(PatternCompileError::Expression { .. })
        ));
    }

    #[test_case(true; "regex_text_limit")]
    #[test_case(false; "regex_nesting_limit")]
    fn regex_resources_are_bounded(text: bool) {
        let expression = if text {
            "a".repeat(MAX_SLOT_EXPRESSION_BYTES + 1)
        } else {
            let depth = REGEX_NEST_LIMIT as usize + 1;
            format!("{}a{}", "(".repeat(depth), ")".repeat(depth))
        };
        assert!(
            CompiledPattern::compile(&pattern(ArgumentDomain::Regex {
                pattern: expression
            }))
            .is_err()
        );
    }

    #[test_case("a*", "alpha", true; "one_argument")]
    #[test_case("a*", "xalpha", false; "whole_argument")]
    #[test_case("a*", "a/child", false; "star_excludes_slash")]
    #[test_case("**/*.rs", "src/deep/file.rs", true; "recursive_star")]
    #[test_case(r"file\*", "file*", true; "escaped_metacharacter")]
    #[test_case(r"file\*", "file1", false; "escape_is_literal")]
    #[test_case("?", "é", false; "question_is_one_utf8_byte")]
    #[test_case("??", "é", true; "two_utf8_bytes")]
    #[test_case("é", "é", true; "literal_unicode")]
    #[test_case("A*", "alpha", false; "case_sensitive")]
    fn glob_semantics_are_explicit(expression: &str, value: &str, expected: bool) {
        let compiled = CompiledPattern::compile(&pattern(ArgumentDomain::Glob {
            pattern: expression.into(),
        }))
        .unwrap();
        let row = observation(&[ARGV[0], ARGV[1], value], "matching");
        assert_eq!(compiled.matches(&row).unwrap().is_match(), expected);
    }

    #[test_case("--config=evil"; "attached_flag")]
    #[test_case("-rf"; "clustered_flag")]
    #[test_case("--"; "option_terminator")]
    fn option_like_values_require_proven_data_and_explicit_opt_in(value: &str) {
        let mut definition = pattern(ArgumentDomain::AnyLiteralArgument);
        let mut row = observation(&[ARGV[0], ARGV[1], value], "matching");
        let compiled = CompiledPattern::compile(&definition).unwrap();
        assert!(matches!(
            compiled.matches(&row).unwrap(),
            MatchReport::NotMatched {
                reason: PatternMismatch::OptionLike { .. }
            }
        ));
        definition.slots[0].option_like = OptionLikePolicy::AllowForProvenData;
        assert!(
            !CompiledPattern::compile(&definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
        definition.argv[2] = PatternToken::Slot {
            id: SLOT,
            role: ArgumentRole::Data,
        };
        row.roles[2] = ArgumentRole::Data;
        assert!(
            CompiledPattern::compile(&definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
    }

    #[test_case("flags"; "appended_flags")]
    #[test_case("extra"; "appended_argument")]
    #[test_case("leading"; "leading_flags")]
    #[test_case("operation"; "different_operation")]
    #[test_case("executable"; "different_executable")]
    #[test_case("role"; "different_role")]
    fn structural_near_misses_are_rejected(change: &str) {
        let compiled =
            CompiledPattern::compile(&pattern(ArgumentDomain::AnyLiteralArgument)).unwrap();
        let mut row = observation(&ARGV, "matching");
        match change {
            "flags" | "extra" => {
                row.argv
                    .push(if change == "flags" { "--fix" } else { "other" }.into());
                row.roles.push(ArgumentRole::Unknown);
            }
            "leading" => {
                row.argv.insert(1, "--config".into());
                row.roles.insert(1, ArgumentRole::Unknown);
            }
            "operation" => row.argv[1] = "push".into(),
            "executable" => row.argv[0] = "./novelctl".into(),
            "role" => row.roles[2] = ArgumentRole::Operation,
            _ => unreachable!(),
        }
        assert!(!compiled.matches(&row).unwrap().is_match());
    }

    #[test_case("tool"; "tool_identity")]
    #[test_case("executable"; "executable_identity")]
    #[test_case("workdir"; "effective_workdir")]
    #[test_case("binding"; "path_binding")]
    #[test_case("analysis"; "analysis_version")]
    fn context_is_pinned(field: &str) {
        let compiled =
            CompiledPattern::compile(&pattern(ArgumentDomain::AnyLiteralArgument)).unwrap();
        let mut row = observation(&ARGV, "matching");
        match field {
            "tool" => row.context.tool_identity.push_str("-other"),
            "executable" => row.context.executable_identity.push_str("-other"),
            "workdir" => row.context.effective_workdir.push_str("-other"),
            "binding" => row.context.path_binding.push_str("-other"),
            "analysis" => row.context.analysis_version.push_str("-other"),
            _ => unreachable!(),
        }
        assert!(matches!(
            compiled.matches(&row).unwrap(),
            MatchReport::NotMatched {
                reason: PatternMismatch::Context { .. }
            }
        ));
    }

    #[test_case("static"; "dynamic_argv")]
    #[test_case("complete"; "source_gap")]
    #[test_case("context"; "unproven_context")]
    #[test_case("sensitivity"; "unchecked_sensitivity")]
    #[test_case("effects"; "unrepresented_effects")]
    #[test_case("unknown"; "unknown_effects")]
    #[test_case("payload"; "interpreter_payload")]
    #[test_case("bytes"; "argument_work_bound")]
    fn unsafe_observations_are_errors_not_matches(field: &str) {
        let compiled =
            CompiledPattern::compile(&pattern(ArgumentDomain::AnyLiteralArgument)).unwrap();
        let mut row = observation(&ARGV, "matching");
        match field {
            "static" => row.verification.static_argv = false,
            "complete" => row.verification.complete_command = false,
            "context" => row.verification.context_verified = false,
            "sensitivity" => row.verification.sensitivity_checked = false,
            "effects" => row.verification.shell_effects = ShellEffectStatus::Present,
            "unknown" => row.verification.shell_effects = ShellEffectStatus::Unknown,
            "payload" => row.roles[2] = ArgumentRole::Payload,
            "bytes" => row.argv[2] = "x".repeat(MAX_ARGUMENT_BYTES + 1),
            _ => unreachable!(),
        }
        assert!(compiled.matches(&row).is_err());
    }

    #[test_case("beta", false; "same_slot_requires_equality")]
    #[test_case("alpha", true; "same_slot_equal_values")]
    fn slot_ids_not_labels_define_equality(second: &str, expected: bool) {
        let mut definition = pattern(ArgumentDomain::AnyLiteralArgument);
        definition.argv.push(PatternToken::Slot {
            id: SLOT,
            role: ArgumentRole::Unknown,
        });
        let row = observation(&[ARGV[0], ARGV[1], ARGV[2], second], "matching");
        assert_eq!(
            CompiledPattern::compile(&definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match(),
            expected
        );
        definition.argv[3] = PatternToken::Slot {
            id: SlotId(3),
            role: ArgumentRole::Unknown,
        };
        let mut independent = definition.slots[0].clone();
        independent.id = SlotId(3);
        definition.slots.push(independent);
        assert!(
            CompiledPattern::compile(&definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
    }

    #[test_case("beta"; "tuple_outside_regex")]
    #[test_case("--config"; "tuple_outside_option_guard")]
    fn inconsistent_tuples_fail_compilation(value: &str) {
        let mut definition = pattern(ArgumentDomain::Regex {
            pattern: "alpha".into(),
        });
        definition.combinations = SlotCombinations::ObservedTuples {
            tuples: [[(SLOT, value.into())].into()].into(),
        };
        assert!(matches!(
            CompiledPattern::compile(&definition),
            Err(PatternCompileError::InvalidTuple(SLOT))
        ));
    }

    #[test_case("beta"; "approval_is_a_frozen_copy")]
    fn editing_a_definition_does_not_change_a_compiled_matcher(value: &str) {
        let mut definition = pattern(ArgumentDomain::ObservedSet {
            values: [ARGV[2].into()].into(),
        });
        let compiled = CompiledPattern::compile(&definition).unwrap();
        let original = compiled.fingerprint().to_owned();
        definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
        let row = observation(&[ARGV[0], ARGV[1], value], "matching");
        assert!(!compiled.matches(&row).unwrap().is_match());
        assert_eq!(compiled.fingerprint(), original);
        assert_ne!(compiled.definition(), &definition);
        assert!(
            CompiledPattern::compile(&definition)
                .unwrap()
                .matches(&row)
                .unwrap()
                .is_match()
        );
    }
}
