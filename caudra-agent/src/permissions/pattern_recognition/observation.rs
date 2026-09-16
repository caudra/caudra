use caudra_storage::permission_patterns::{
    ArgumentRole, MAX_ARGV_BYTES, MAX_PATTERN_ARGV, PatternContext, PatternValidationError,
    validate_argument, validate_argument_role,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;
pub const MAX_OBSERVATION_JSON_BYTES: usize = 256 * 1024;
pub const MAX_OBSERVATION_ID_BYTES: usize = 256;
pub const MAX_TIMESTAMP_MS: u64 = 253_402_300_799_999;
const SHA256_HEX_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationProvenance {
    Native,
    Imported,
    Legacy,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcome {
    Requested,
    Succeeded,
    Failed,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellEffectStatus {
    Absent,
    Present,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationVerification {
    pub complete_command: bool,
    pub static_argv: bool,
    pub context_verified: bool,
    pub sensitivity_checked: bool,
    pub shell_effects: ShellEffectStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationSource {
    pub source_identity: String,
    pub observation_id: String,
    pub input_hash: String,
    pub session_id: String,
    pub timestamp_ms: u64,
    pub provenance: ObservationProvenance,
    pub outcome: InvocationOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandObservation {
    pub version: u16,
    pub argv: Vec<String>,
    pub roles: Vec<ArgumentRole>,
    pub context: PatternContext,
    pub verification: ObservationVerification,
    pub source: ObservationSource,
}

#[derive(Debug, Error)]
pub enum ObservationError {
    #[error("unsupported command observation version {0}")]
    UnsupportedVersion(u16),
    #[error(
        "command facts are incomplete, dynamic, unverified, or have present/unknown shell effects"
    )]
    UnverifiedFacts,
    #[error("sensitive arguments or interpreted payloads are excluded from pattern recognition")]
    SensitiveOrPayload,
    #[error("observation argv/role counts are inconsistent or exceed their bounds")]
    InvalidArgv,
    #[error("invalid or missing observation metadata: {0}")]
    InvalidMetadata(&'static str),
    #[error("observation exceeds the JSON byte limit")]
    JsonLimit,
    #[error(transparent)]
    Definition(#[from] PatternValidationError),
    #[error("invalid command observation JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl CommandObservation {
    pub fn from_json(json: &str) -> Result<Self, ObservationError> {
        if json.len() > MAX_OBSERVATION_JSON_BYTES {
            return Err(ObservationError::JsonLimit);
        }
        let observation: Self = serde_json::from_str(json)?;
        observation.validate()?;
        Ok(observation)
    }

    pub fn validate(&self) -> Result<(), ObservationError> {
        if self.version != OBSERVATION_SCHEMA_VERSION {
            return Err(ObservationError::UnsupportedVersion(self.version));
        }
        let verification = &self.verification;
        if !verification.complete_command
            || !verification.static_argv
            || !verification.context_verified
            || !verification.sensitivity_checked
            || verification.shell_effects != ShellEffectStatus::Absent
        {
            return Err(ObservationError::UnverifiedFacts);
        }
        if self.argv.is_empty()
            || self.argv.len() > MAX_PATTERN_ARGV
            || self.argv.len() != self.roles.len()
            || self.roles.first() != Some(&ArgumentRole::Executable)
        {
            return Err(ObservationError::InvalidArgv);
        }
        let mut bytes = 0;
        for (index, (value, role)) in self.argv.iter().zip(&self.roles).enumerate() {
            if matches!(role, ArgumentRole::Sensitive | ArgumentRole::Payload) {
                return Err(ObservationError::SensitiveOrPayload);
            }
            validate_argument(value, index)?;
            validate_argument_role(value, role, index)?;
            bytes += value.len();
        }
        if bytes > MAX_ARGV_BYTES {
            return Err(ObservationError::InvalidArgv);
        }
        self.context.validate()?;
        for (field, value) in [
            ("source_identity", &self.source.source_identity),
            ("observation_id", &self.source.observation_id),
            ("session_id", &self.source.session_id),
        ] {
            if value.is_empty()
                || value.len() > MAX_OBSERVATION_ID_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(ObservationError::InvalidMetadata(field));
            }
        }
        if self.source.input_hash.len() != SHA256_HEX_BYTES
            || !self
                .source
                .input_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ObservationError::InvalidMetadata("input_hash"));
        }
        if self.source.timestamp_ms == 0 || self.source.timestamp_ms > MAX_TIMESTAMP_MS {
            return Err(ObservationError::InvalidMetadata("timestamp_ms"));
        }
        if self.source.provenance == ObservationProvenance::Unknown {
            return Err(ObservationError::InvalidMetadata("provenance"));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{
        CommandObservation, InvocationOutcome, OBSERVATION_SCHEMA_VERSION, ObservationProvenance,
        ObservationSource, ObservationVerification, ShellEffectStatus,
    };
    use caudra_storage::permission_patterns::{ArgumentRole, PatternContext};

    pub const NOW_MS: u64 = 10_000;
    pub const INPUT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    pub fn observation(argv: &[&str], id: &str) -> CommandObservation {
        CommandObservation {
            version: OBSERVATION_SCHEMA_VERSION,
            argv: argv.iter().map(|value| (*value).into()).collect(),
            roles: argv
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    if index == 0 {
                        ArgumentRole::Executable
                    } else {
                        ArgumentRole::Unknown
                    }
                })
                .collect(),
            context: PatternContext {
                tool_identity: "native/shell/v1".into(),
                executable_identity: "trusted/executable".into(),
                effective_workdir: "/project".into(),
                path_binding: "local/project".into(),
                analysis_version: "static/v1".into(),
            },
            verification: ObservationVerification {
                complete_command: true,
                static_argv: true,
                context_verified: true,
                sensitivity_checked: true,
                shell_effects: ShellEffectStatus::Absent,
            },
            source: ObservationSource {
                source_identity: "test-corpus".into(),
                observation_id: id.into(),
                input_hash: INPUT_HASH.into(),
                session_id: id.into(),
                timestamp_ms: NOW_MS,
                provenance: ObservationProvenance::Native,
                outcome: InvocationOutcome::Requested,
            },
        }
    }
}
