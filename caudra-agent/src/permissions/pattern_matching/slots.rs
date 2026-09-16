use caudra_storage::permission_patterns::{ArgumentDomain, PatternSlot, SlotId};
use globset::GlobBuilder;
use regex::{
    Regex, RegexBuilder,
    bytes::{Regex as BytesRegex, RegexBuilder as BytesRegexBuilder},
};

use super::PatternCompileError;

pub const REGEX_NEST_LIMIT: u32 = 32;
pub const REGEX_SIZE_LIMIT: usize = 256 * 1024;
pub const REGEX_DFA_SIZE_LIMIT: usize = 256 * 1024;
pub const MAX_GLOB_REGEX_BYTES: usize = 16 * 1024;
pub const GLOB_ARGUMENT_SEMANTICS: &str = "Case-sensitive UTF-8 bytes; ? matches one byte, * excludes literal /, ** may cross /, backslash escapes metacharacters, braces/classes use globset syntax; one entire argument, no shell or filesystem expansion.";

pub(super) enum CompiledExpression {
    Regex(Regex),
    Glob(BytesRegex),
    None,
}

impl CompiledExpression {
    pub(super) fn compile(slot: &PatternSlot) -> Result<Self, PatternCompileError> {
        match &slot.domain {
            ArgumentDomain::Regex { pattern } => {
                compile_regex(pattern, slot.id)?;
                compile_regex(&format!(r"\A(?:{pattern})\z"), slot.id).map(Self::Regex)
            }
            ArgumentDomain::Glob { pattern } => {
                let glob = GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .case_insensitive(false)
                    .empty_alternates(false)
                    .build()
                    .map_err(|error| invalid_expression(slot.id, error.to_string()))?;
                if glob.regex().len() > MAX_GLOB_REGEX_BYTES {
                    return Err(invalid_expression(
                        slot.id,
                        "generated glob exceeds regex byte limit".into(),
                    ));
                }
                BytesRegexBuilder::new(&format!(r"\A(?:{})\z", glob.regex()))
                    .size_limit(REGEX_SIZE_LIMIT)
                    .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
                    .nest_limit(REGEX_NEST_LIMIT)
                    .build()
                    .map(Self::Glob)
                    .map_err(|error| invalid_expression(slot.id, error.to_string()))
            }
            ArgumentDomain::ObservedSet { .. }
            | ArgumentDomain::Exact { .. }
            | ArgumentDomain::AnyLiteralArgument => Ok(Self::None),
        }
    }

    pub(super) fn matches(&self, domain: &ArgumentDomain, value: &str) -> bool {
        match (self, domain) {
            (Self::Regex(regex), ArgumentDomain::Regex { .. }) => regex.is_match(value),
            (Self::Glob(regex), ArgumentDomain::Glob { .. }) => regex.is_match(value.as_bytes()),
            (Self::None, ArgumentDomain::ObservedSet { values }) => values.contains(value),
            (Self::None, ArgumentDomain::Exact { value: exact }) => exact == value,
            (Self::None, ArgumentDomain::AnyLiteralArgument) => true,
            _ => false,
        }
    }
}

fn compile_regex(pattern: &str, slot: SlotId) -> Result<Regex, PatternCompileError> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .nest_limit(REGEX_NEST_LIMIT)
        .build()
        .map_err(|error| invalid_expression(slot, error.to_string()))
}

fn invalid_expression(slot: SlotId, reason: String) -> PatternCompileError {
    PatternCompileError::Expression { slot, reason }
}
