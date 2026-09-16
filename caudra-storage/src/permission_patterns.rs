use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::num::ParseIntError;
use thiserror::Error;

pub const PATTERN_SCHEMA_VERSION: u16 = 1;
pub const MAX_PATTERN_JSON_BYTES: usize = 256 * 1024;
pub const MAX_PATTERN_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_PATTERN_ARGV: usize = 64;
pub const MAX_PATTERN_SLOTS: usize = 16;
pub const MAX_PATTERN_TUPLES: usize = 128;
pub const MAX_DOMAIN_VALUES: usize = 128;
pub const MAX_ARGUMENT_BYTES: usize = 4096;
pub const MAX_ARGV_BYTES: usize = 32 * 1024;
pub const MAX_CONTEXT_FIELD_BYTES: usize = 4096;
pub const MAX_PATTERN_LABEL_BYTES: usize = 128;
pub const MAX_SLOT_EXPRESSION_BYTES: usize = 1024;

pub type ObservedTuple = BTreeMap<SlotId, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct SlotId(pub u16);

impl From<SlotId> for String {
    fn from(id: SlotId) -> Self {
        id.0.to_string()
    }
}

impl TryFrom<String> for SlotId {
    type Error = ParseIntError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse().map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentRole {
    Executable,
    Operation,
    Flag,
    OptionTerminator,
    Data,
    Unknown,
    Sensitive,
    Payload,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternContext {
    pub tool_identity: String,
    pub executable_identity: String,
    pub effective_workdir: String,
    pub path_binding: String,
    pub analysis_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatternToken {
    Exact { value: String, role: ArgumentRole },
    Slot { id: SlotId, role: ArgumentRole },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArgumentDomain {
    ObservedSet { values: BTreeSet<String> },
    Exact { value: String },
    Glob { pattern: String },
    Regex { pattern: String },
    AnyLiteralArgument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionLikePolicy {
    Reject,
    AllowForProvenData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternSlot {
    pub id: SlotId,
    pub label: String,
    pub domain: ArgumentDomain,
    pub option_like: OptionLikePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SlotCombinations {
    ObservedTuples { tuples: BTreeSet<ObservedTuple> },
    Independent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternDefinition {
    pub version: u16,
    pub name: String,
    pub context: PatternContext,
    pub argv: Vec<PatternToken>,
    pub slots: Vec<PatternSlot>,
    pub combinations: SlotCombinations,
}

#[derive(Debug, Error)]
pub enum PatternValidationError {
    #[error("unsupported permission pattern version {0}")]
    UnsupportedVersion(u16),
    #[error("{field} exceeds its limit of {limit}")]
    LimitExceeded { field: &'static str, limit: usize },
    #[error("invalid or empty context field: {0}")]
    InvalidContext(&'static str),
    #[error("invalid pattern or slot label")]
    InvalidLabel,
    #[error("the executable must be a nonempty exact token at argv index zero")]
    InvalidExecutable,
    #[error("invalid literal argument at argv index {0}")]
    InvalidArgument(usize),
    #[error("inconsistent, sensitive, or payload role at argv index {0}")]
    InvalidRole(usize),
    #[error("slot {0:?} is duplicated, undefined, unused, or attached to a structural argument")]
    InvalidSlot(SlotId),
    #[error("slot {0:?} has an empty or invalid domain")]
    InvalidDomain(SlotId),
    #[error("observed tuples must be nonempty and contain exactly the declared slot IDs")]
    InvalidTuples,
    #[error("invalid permission pattern JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl PatternContext {
    pub fn validate(&self) -> Result<(), PatternValidationError> {
        for (field, value) in self.fields() {
            if value.is_empty()
                || value.len() > MAX_CONTEXT_FIELD_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(PatternValidationError::InvalidContext(field));
            }
        }
        Ok(())
    }

    pub fn fields(&self) -> [(&'static str, &str); 5] {
        [
            ("tool_identity", &self.tool_identity),
            ("executable_identity", &self.executable_identity),
            ("effective_workdir", &self.effective_workdir),
            ("path_binding", &self.path_binding),
            ("analysis_version", &self.analysis_version),
        ]
    }
}

impl PatternDefinition {
    pub fn from_json(json: &str) -> Result<Self, PatternValidationError> {
        check_limit("pattern JSON bytes", json.len(), MAX_PATTERN_JSON_BYTES)?;
        let definition: Self = serde_json::from_str(json)?;
        definition.validate()?;
        Ok(definition)
    }

    pub fn validate(&self) -> Result<(), PatternValidationError> {
        if self.version != PATTERN_SCHEMA_VERSION {
            return Err(PatternValidationError::UnsupportedVersion(self.version));
        }
        self.context.validate()?;
        validate_label(&self.name)?;
        check_limit("argv length", self.argv.len(), MAX_PATTERN_ARGV)?;
        check_limit("slot count", self.slots.len(), MAX_PATTERN_SLOTS)?;
        if !matches!(self.argv.first(), Some(PatternToken::Exact { value, role: ArgumentRole::Executable }) if !value.is_empty() && !is_option_like(value))
        {
            return Err(PatternValidationError::InvalidExecutable);
        }
        let mut bytes = self.name.len();
        for (_, value) in self.context.fields() {
            add_bytes(&mut bytes, value)?;
        }
        let mut slots = BTreeMap::new();
        for slot in &self.slots {
            if slots.insert(slot.id, slot).is_some() {
                return Err(PatternValidationError::InvalidSlot(slot.id));
            }
            validate_label(&slot.label)?;
            add_bytes(&mut bytes, &slot.label)?;
            validate_domain(slot, &mut bytes)?;
        }
        let mut used = BTreeSet::new();
        let mut argv_bytes = 0;
        for (index, token) in self.argv.iter().enumerate() {
            match token {
                PatternToken::Exact { value, role } => {
                    validate_argument(value, index)?;
                    validate_argument_role(value, role, index)?;
                    argv_bytes += value.len();
                    add_bytes(&mut bytes, value)?;
                }
                PatternToken::Slot { id, role } => {
                    if !slots.contains_key(id)
                        || !matches!(role, ArgumentRole::Data | ArgumentRole::Unknown)
                    {
                        return Err(PatternValidationError::InvalidSlot(*id));
                    }
                    used.insert(*id);
                }
            }
        }
        check_limit("argv bytes", argv_bytes, MAX_ARGV_BYTES)?;
        if let Some(id) = slots.keys().find(|id| !used.contains(id)) {
            return Err(PatternValidationError::InvalidSlot(*id));
        }
        if let SlotCombinations::ObservedTuples { tuples } = &self.combinations {
            check_limit("tuple count", tuples.len(), MAX_PATTERN_TUPLES)?;
            if tuples.is_empty() {
                return Err(PatternValidationError::InvalidTuples);
            }
            for tuple in tuples {
                if tuple.len() != slots.len() || !tuple.keys().eq(slots.keys()) {
                    return Err(PatternValidationError::InvalidTuples);
                }
                for (id, value) in tuple {
                    validate_argument(value, usize::from(id.0))?;
                    add_bytes(&mut bytes, value)?;
                    let slot = slots[id];
                    if !finite_domain_contains(&slot.domain, value) {
                        return Err(PatternValidationError::InvalidDomain(*id));
                    }
                }
            }
        }
        check_limit(
            "pattern JSON bytes",
            serde_json::to_vec(self)?.len(),
            MAX_PATTERN_JSON_BYTES,
        )
    }

    pub fn fingerprint(&self) -> Result<String, PatternValidationError> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.name.clear();
        for slot in &mut canonical.slots {
            slot.label.clear();
        }
        canonical.slots.sort_by_key(|slot| slot.id);
        let digest = Sha256::digest(serde_json::to_vec(&canonical)?);
        Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

pub fn validate_argument(value: &str, index: usize) -> Result<(), PatternValidationError> {
    if value.len() > MAX_ARGUMENT_BYTES || value.contains('\0') {
        return Err(PatternValidationError::InvalidArgument(index));
    }
    Ok(())
}

pub fn validate_argument_role(
    value: &str,
    role: &ArgumentRole,
    index: usize,
) -> Result<(), PatternValidationError> {
    let valid = match role {
        ArgumentRole::Executable => index == 0 && !value.is_empty() && !is_option_like(value),
        ArgumentRole::Operation => index != 0 && !value.is_empty() && !is_option_like(value),
        ArgumentRole::Flag => index != 0 && is_option_like(value) && value != "--",
        ArgumentRole::OptionTerminator => index != 0 && value == "--",
        ArgumentRole::Data | ArgumentRole::Unknown => index != 0,
        ArgumentRole::Sensitive | ArgumentRole::Payload => false,
    };
    if !valid {
        return Err(PatternValidationError::InvalidRole(index));
    }
    Ok(())
}

pub fn is_option_like(value: &str) -> bool {
    value.starts_with('-')
}

pub fn finite_domain_contains(domain: &ArgumentDomain, value: &str) -> bool {
    match domain {
        ArgumentDomain::ObservedSet { values } => values.contains(value),
        ArgumentDomain::Exact { value: exact } => exact == value,
        ArgumentDomain::Regex { .. }
        | ArgumentDomain::Glob { .. }
        | ArgumentDomain::AnyLiteralArgument => true,
    }
}

fn validate_label(label: &str) -> Result<(), PatternValidationError> {
    if label.is_empty()
        || label.len() > MAX_PATTERN_LABEL_BYTES
        || label.chars().any(char::is_control)
    {
        return Err(PatternValidationError::InvalidLabel);
    }
    Ok(())
}

fn validate_domain(slot: &PatternSlot, bytes: &mut usize) -> Result<(), PatternValidationError> {
    match &slot.domain {
        ArgumentDomain::ObservedSet { values } => {
            check_limit("domain values", values.len(), MAX_DOMAIN_VALUES)?;
            if values.is_empty() {
                return Err(PatternValidationError::InvalidDomain(slot.id));
            }
            for value in values {
                validate_argument(value, usize::from(slot.id.0))?;
                add_bytes(bytes, value)?;
            }
        }
        ArgumentDomain::Exact { value } => {
            validate_argument(value, usize::from(slot.id.0))?;
            add_bytes(bytes, value)?;
        }
        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
            check_limit(
                "slot expression bytes",
                pattern.len(),
                MAX_SLOT_EXPRESSION_BYTES,
            )?;
            if pattern.contains('\0') {
                return Err(PatternValidationError::InvalidDomain(slot.id));
            }
            add_bytes(bytes, pattern)?;
        }
        ArgumentDomain::AnyLiteralArgument => {}
    }
    Ok(())
}

fn add_bytes(bytes: &mut usize, value: &str) -> Result<(), PatternValidationError> {
    *bytes += value.len();
    check_limit("pattern value bytes", *bytes, MAX_PATTERN_VALUE_BYTES)
}

fn check_limit(
    field: &'static str,
    value: usize,
    limit: usize,
) -> Result<(), PatternValidationError> {
    if value > limit {
        return Err(PatternValidationError::LimitExceeded { field, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ArgumentDomain, ArgumentRole, MAX_PATTERN_JSON_BYTES, OptionLikePolicy,
        PATTERN_SCHEMA_VERSION, PatternContext, PatternDefinition, PatternSlot, PatternToken,
        PatternValidationError, SlotCombinations, SlotId,
    };
    use test_case::test_case;

    fn definition() -> PatternDefinition {
        PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: "Observed command pattern".into(),
            context: PatternContext {
                tool_identity: "native/shell/v1".into(),
                executable_identity: "/bin/unknown-cli".into(),
                effective_workdir: "/project".into(),
                path_binding: "local/project".into(),
                analysis_version: "static/v1".into(),
            },
            argv: vec![
                PatternToken::Exact {
                    value: "unknown-cli".into(),
                    role: ArgumentRole::Executable,
                },
                PatternToken::Slot {
                    id: SlotId(1),
                    role: ArgumentRole::Data,
                },
            ],
            slots: vec![PatternSlot {
                id: SlotId(1),
                label: "<pattern1>".into(),
                domain: ArgumentDomain::ObservedSet {
                    values: ["alpha".into()].into(),
                },
                option_like: OptionLikePolicy::Reject,
            }],
            combinations: SlotCombinations::ObservedTuples {
                tuples: [[(SlotId(1), "alpha".into())].into()].into(),
            },
        }
    }

    #[test_case("renamed", "<package>"; "labels_are_not_authority")]
    fn renaming_preserves_fingerprint(name: &str, label: &str) {
        let mut pattern = definition();
        let original = pattern.fingerprint().unwrap();
        pattern.name = name.into();
        pattern.slots[0].label = label.into();
        assert_eq!(original, pattern.fingerprint().unwrap());
        let json = serde_json::to_string(&pattern).unwrap();
        assert_eq!(PatternDefinition::from_json(&json).unwrap(), pattern);
        pattern.combinations = SlotCombinations::Independent;
        assert_ne!(original, pattern.fingerprint().unwrap());
    }

    #[test_case("workdir"; "context_is_authority")]
    #[test_case("domain"; "domain_is_authority")]
    #[test_case("guard"; "option_guard_is_authority")]
    fn constraints_change_fingerprint(field: &str) {
        let mut pattern = definition();
        let original = pattern.fingerprint().unwrap();
        match field {
            "workdir" => pattern.context.effective_workdir = "/other".into(),
            "domain" => pattern.slots[0].domain = ArgumentDomain::AnyLiteralArgument,
            "guard" => pattern.slots[0].option_like = OptionLikePolicy::AllowForProvenData,
            _ => unreachable!(),
        }
        assert_ne!(original, pattern.fingerprint().unwrap());
    }

    #[test_case("{}"; "missing_fields")]
    #[test_case("{\"version\":1,\"eval\":\"anything\"}"; "unknown_fields")]
    fn invalid_schema_is_rejected(json: &str) {
        assert!(PatternDefinition::from_json(json).is_err());
    }

    #[test_case(0; "zero_version")]
    #[test_case(PATTERN_SCHEMA_VERSION + 1; "future_version")]
    fn unsupported_versions_are_rejected(version: u16) {
        let mut pattern = definition();
        pattern.version = version;
        assert!(matches!(
            pattern.validate(),
            Err(PatternValidationError::UnsupportedVersion(_))
        ));
    }

    #[test_case(true; "undefined_slot")]
    #[test_case(false; "incomplete_tuple")]
    fn slot_integrity_is_validated(undefined: bool) {
        let mut pattern = definition();
        if undefined {
            pattern.slots[0].id = SlotId(2);
        } else {
            pattern.combinations = SlotCombinations::ObservedTuples {
                tuples: [Default::default()].into(),
            };
        }
        assert!(pattern.validate().is_err());
    }

    #[test_case(MAX_PATTERN_JSON_BYTES + 1; "bounded_before_deserialization")]
    fn json_is_bounded(bytes: usize) {
        assert!(matches!(
            PatternDefinition::from_json(&" ".repeat(bytes)),
            Err(PatternValidationError::LimitExceeded { .. })
        ));
    }
}
