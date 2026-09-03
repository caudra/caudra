use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::CaudraId;
use crate::state::{SCOPE_GLOBAL, StateKey, StateStore};
use crate::{StateClass, StateDir, StorageError, now_epoch};

const PERMISSION_RULES: StateKey = StateKey {
    name: "permission.rules",
    class: StateClass::Persistent,
};
pub const COMMAND_PATTERN_MAX_BYTES: usize = 256;
pub const COMMAND_PATTERN_MAX_TOKENS: usize = 8;
const SHA256_HEX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionSubject {
    Native {
        owner: String,
        contract: String,
    },
    Lua {
        plugin: String,
        tool: String,
        contract: String,
    },
    Mcp {
        #[serde(default)]
        server: String,
        authority: String,
        tool: String,
        contract: String,
    },
    UnknownLegacy {
        identity: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionExecutorKind {
    Native,
    Lua,
    Mcp,
    UnknownLegacy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionResourceKind {
    File,
    Directory,
    Url,
    Command,
    Query,
    Custom { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionResourceAccess {
    Read,
    Write,
    Execute,
    Search,
    Connect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "match", rename_all = "snake_case")]
pub enum PermissionResourceSelector {
    Exact { value: String },
    Digest { digest: String },
    FilesystemSubtreeDigest { digest: String },
    UrlSubtreeDigest { digest: String },
    UrlOriginDigest { digest: String },
    CommandPattern { pattern: String },
    Subtree { root: String },
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResourceConstraint {
    pub kind: PermissionResourceKind,
    pub selector: PermissionResourceSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<PermissionResourceAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, PermissionResourceSelector>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectedPermissionArgument {
    pub pointer: String,
    pub value: Value,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "constraint", rename_all = "snake_case")]
pub enum PermissionArgumentConstraint {
    Exact {
        digest: String,
    },
    Selected {
        arguments: Vec<SelectedPermissionArgument>,
    },
    SelectedDigest {
        pointers: Vec<String>,
        digest: String,
    },
    Unconstrained,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLifetime {
    Once,
    Conversation,
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredPermissionEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredPermissionRule {
    pub subject: PermissionSubject,
    pub executor: PermissionExecutorKind,
    pub resources: Vec<PermissionResourceConstraint>,
    pub arguments: PermissionArgumentConstraint,
    pub lifetime: PermissionLifetime,
    pub effect: StructuredPermissionEffect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    pub rule: StructuredPermissionRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Value>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<u64>,
}

impl PermissionRuleRecord {
    pub fn conversation(rule: StructuredPermissionRule) -> Result<Self, PermissionStateError> {
        Self::conversation_with_review(rule, None)
    }

    pub fn conversation_with_review(
        rule: StructuredPermissionRule,
        review: Option<Value>,
    ) -> Result<Self, PermissionStateError> {
        let record = Self {
            id: CaudraId::generate().to_string(),
            project: None,
            rule,
            review,
            created_at: now_epoch(),
            revoked_at: None,
        };
        validate_record(&record, false)?;
        Ok(record)
    }

    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionStateError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("invalid permission state: {0}")]
    Invalid(String),
}

pub struct PermissionState {
    store: StateStore,
    existed: bool,
    records: Vec<PermissionRuleRecord>,
}

impl PermissionState {
    pub fn open(state_dir: &StateDir) -> Result<Self, PermissionStateError> {
        let store = StateStore::open(state_dir, PERMISSION_RULES.class)?;
        let records = store.get::<Vec<PermissionRuleRecord>>(SCOPE_GLOBAL, PERMISSION_RULES)?;
        let existed = records.is_some();
        let records = records.unwrap_or_default();
        validate_records(&records)?;
        Ok(Self {
            store,
            existed,
            records,
        })
    }

    pub fn refresh(&mut self) -> Result<(), PermissionStateError> {
        match self
            .store
            .get::<Vec<PermissionRuleRecord>>(SCOPE_GLOBAL, PERMISSION_RULES)?
        {
            Some(records) => {
                validate_records(&records)?;
                self.records = records;
                self.existed = true;
                Ok(())
            }
            None if !self.existed => Ok(()),
            None => Err(PermissionStateError::Invalid(
                "permission state row disappeared".into(),
            )),
        }
    }

    pub fn records(&self) -> &[PermissionRuleRecord] {
        &self.records
    }

    pub fn insert(
        &mut self,
        project: Option<PathBuf>,
        rule: StructuredPermissionRule,
    ) -> Result<PermissionRuleRecord, PermissionStateError> {
        self.insert_with_review(project, rule, None)
    }

    pub fn insert_with_review(
        &mut self,
        project: Option<PathBuf>,
        rule: StructuredPermissionRule,
        review: Option<Value>,
    ) -> Result<PermissionRuleRecord, PermissionStateError> {
        let record = PermissionRuleRecord {
            id: CaudraId::generate().to_string(),
            project,
            rule,
            review,
            created_at: now_epoch(),
            revoked_at: None,
        };
        validate_record(&record, true)?;
        let inserted = record.clone();
        self.records = self.store.try_update(
            SCOPE_GLOBAL,
            PERMISSION_RULES,
            |records: &mut Vec<PermissionRuleRecord>| -> Result<_, PermissionStateError> {
                validate_records(records)?;
                records.push(inserted);
                validate_records(records)?;
                Ok(records.clone())
            },
        )??;
        self.existed = true;
        Ok(record)
    }

    pub fn revoke(&mut self, id: &str) -> Result<bool, PermissionStateError> {
        let (revoked, records) = self.store.try_update(
            SCOPE_GLOBAL,
            PERMISSION_RULES,
            |records: &mut Vec<PermissionRuleRecord>| -> Result<_, PermissionStateError> {
                validate_records(records)?;
                let Some(record) = records
                    .iter_mut()
                    .find(|record| record.id == id && record.is_active())
                else {
                    return Ok((false, records.clone()));
                };
                record.revoked_at = Some(now_epoch());
                Ok((true, records.clone()))
            },
        )??;
        self.records = records;
        self.existed = true;
        Ok(revoked)
    }
}

pub fn validate_conversation_record(
    record: &PermissionRuleRecord,
) -> Result<(), PermissionStateError> {
    validate_record(record, false)
}

fn validate_records(records: &[PermissionRuleRecord]) -> Result<(), PermissionStateError> {
    let mut ids = HashSet::with_capacity(records.len());
    for record in records {
        validate_record(record, true)?;
        if !ids.insert(&record.id) {
            return Err(PermissionStateError::Invalid(format!(
                "duplicate record ID {:?}",
                record.id
            )));
        }
    }
    Ok(())
}

fn validate_record(
    record: &PermissionRuleRecord,
    persistent: bool,
) -> Result<(), PermissionStateError> {
    record
        .id
        .parse::<CaudraId>()
        .map_err(|error| PermissionStateError::Invalid(format!("invalid record ID: {error}")))?;
    if record
        .revoked_at
        .is_some_and(|revoked_at| revoked_at < record.created_at)
    {
        return Err(PermissionStateError::Invalid(
            "revocation predates rule creation".into(),
        ));
    }
    match (&record.rule.lifetime, &record.project, persistent) {
        (PermissionLifetime::Conversation, None, false) => {}
        (PermissionLifetime::Project, Some(project), true) if project.is_absolute() => {}
        (PermissionLifetime::Global, None, true) => {}
        _ => {
            return Err(PermissionStateError::Invalid(
                "rule lifetime does not match its storage scope".into(),
            ));
        }
    }
    match &record.rule.arguments {
        PermissionArgumentConstraint::Exact { digest } => validate_digest(digest)?,
        PermissionArgumentConstraint::Selected { .. } => {
            return Err(PermissionStateError::Invalid(
                "selected raw arguments cannot be stored durably".into(),
            ));
        }
        PermissionArgumentConstraint::SelectedDigest { pointers, digest } => {
            if pointers.is_empty() || pointers.iter().any(|pointer| pointer.is_empty()) {
                return Err(PermissionStateError::Invalid(
                    "selected argument pointers must be non-empty".into(),
                ));
            }
            let mut unique = HashSet::with_capacity(pointers.len());
            if pointers.iter().any(|pointer| !unique.insert(pointer)) {
                return Err(PermissionStateError::Invalid(
                    "selected argument pointers must be unique".into(),
                ));
            }
            validate_digest(digest)?;
        }
        PermissionArgumentConstraint::Unconstrained => {}
    }
    for resource in &record.rule.resources {
        validate_resource_selector(&resource.kind, &resource.selector)?;
        for (attribute, selector) in &resource.attributes {
            if matches!(selector, PermissionResourceSelector::CommandPattern { .. }) {
                return Err(PermissionStateError::Invalid(format!(
                    "command pattern selector cannot be used for resource attribute {attribute:?}"
                )));
            }
            validate_selector(selector)?;
        }
    }
    if let Some(review) = &record.review {
        validate_review(review)?;
    }
    Ok(())
}

fn validate_review(value: &Value) -> Result<(), PermissionStateError> {
    match value {
        Value::String(value) if value.starts_with('<') && value.ends_with('>') => Ok(()),
        Value::Array(values) => values.iter().try_for_each(validate_review),
        Value::Object(values)
            if values.keys().all(|key| {
                key == "<omitted>" || key.starts_with("<field:") && key.ends_with('>')
            }) =>
        {
            values.values().try_for_each(validate_review)
        }
        _ => Err(PermissionStateError::Invalid(
            "permission review contains an unredacted value".into(),
        )),
    }
}

fn validate_resource_selector(
    kind: &PermissionResourceKind,
    selector: &PermissionResourceSelector,
) -> Result<(), PermissionStateError> {
    if let PermissionResourceSelector::CommandPattern { pattern } = selector {
        if !matches!(kind, PermissionResourceKind::Command) {
            return Err(PermissionStateError::Invalid(
                "command pattern selector requires a command resource".into(),
            ));
        }
        return validate_command_pattern(pattern).map_err(PermissionStateError::Invalid);
    }
    validate_selector(selector)
}

fn validate_selector(selector: &PermissionResourceSelector) -> Result<(), PermissionStateError> {
    match selector {
        PermissionResourceSelector::Digest { digest }
        | PermissionResourceSelector::FilesystemSubtreeDigest { digest }
        | PermissionResourceSelector::UrlSubtreeDigest { digest }
        | PermissionResourceSelector::UrlOriginDigest { digest } => validate_digest(digest),
        PermissionResourceSelector::Any => Ok(()),
        PermissionResourceSelector::CommandPattern { .. } => Err(PermissionStateError::Invalid(
            "command pattern selector is only valid as a primary command resource selector".into(),
        )),
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Subtree { .. } => {
            Err(PermissionStateError::Invalid(
                "raw resource values cannot be stored durably".into(),
            ))
        }
    }
}

pub fn validate_command_pattern(pattern: &str) -> Result<(), String> {
    if pattern.len() > COMMAND_PATTERN_MAX_BYTES {
        return Err(format!(
            "command pattern exceeds {COMMAND_PATTERN_MAX_BYTES} UTF-8 bytes"
        ));
    }
    if pattern.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("command pattern contains a control character".into());
    }
    if !pattern.is_ascii() {
        return Err("command pattern must contain only ASCII characters".into());
    }

    let tokens: Vec<_> = pattern.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > COMMAND_PATTERN_MAX_TOKENS {
        return Err(format!(
            "command pattern must contain 1 to {COMMAND_PATTERN_MAX_TOKENS} tokens"
        ));
    }
    for (index, token) in tokens.iter().enumerate() {
        if *token == "*" {
            if index + 1 != tokens.len() {
                return Err("command pattern wildcard must be the final token".into());
            }
            continue;
        }
        if token.contains('*') {
            return Err(
                "command pattern wildcard must be a bare final token separated by a space".into(),
            );
        }
        if !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'/' | b'@' | b':' | b'=' | b'+' | b'-')
        }) {
            return Err("command pattern literal token contains an invalid character".into());
        }
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), PermissionStateError> {
    if digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(PermissionStateError::Invalid(
            "invalid SHA-256 digest".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use serde_json::json;

    use super::{
        COMMAND_PATTERN_MAX_BYTES, PERMISSION_RULES, PermissionArgumentConstraint,
        PermissionExecutorKind, PermissionLifetime, PermissionResourceAccess,
        PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
        PermissionRuleRecord, PermissionState, PermissionStateError, PermissionSubject,
        SHA256_HEX_LEN, StructuredPermissionEffect, StructuredPermissionRule,
        validate_command_pattern,
    };
    use crate::state::{self, SCOPE_GLOBAL};
    use crate::{StateDir, now_epoch};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn rule(lifetime: PermissionLifetime) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: "caudra".into(),
                contract: "bash".into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Command,
                selector: PermissionResourceSelector::Digest {
                    digest: DIGEST.into(),
                },
                access: Some(PermissionResourceAccess::Execute),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            arguments: PermissionArgumentConstraint::Exact {
                digest: DIGEST.into(),
            },
            lifetime,
            effect: StructuredPermissionEffect::Allow,
        }
    }

    fn command_pattern_rule(
        lifetime: PermissionLifetime,
        pattern: &str,
    ) -> StructuredPermissionRule {
        let mut rule = rule(lifetime);
        rule.resources[0].selector = PermissionResourceSelector::CommandPattern {
            pattern: pattern.into(),
        };
        rule
    }

    fn invalid_message(error: PermissionStateError) -> String {
        match error {
            PermissionStateError::Invalid(message) => message,
            other => panic!("expected invalid permission state, got {other}"),
        }
    }

    #[test]
    fn command_patterns_accept_conservative_tokens_and_final_wildcard() {
        let maximum_length = "a".repeat(COMMAND_PATTERN_MAX_BYTES);
        for pattern in [
            "git",
            "git status",
            "git status *",
            "cargo-test_1 ./src /tmp/foo user@host key=value +flag foo:bar",
            "one two three four five six seven *",
            maximum_length.as_str(),
        ] {
            assert!(
                validate_command_pattern(pattern).is_ok(),
                "pattern should be accepted: {pattern:?}"
            );
        }
    }

    #[test]
    fn command_patterns_reject_unsafe_or_ambiguous_syntax() {
        let too_long = "a".repeat(COMMAND_PATTERN_MAX_BYTES + 1);
        for pattern in [
            "",
            "   ",
            "one two three four five six seven eight nine",
            "git status*",
            "git * status",
            "git **",
            "git \"status\"",
            "git\tstatus",
            "git\nstatus",
            "git café",
            "git $(pwd)",
            too_long.as_str(),
        ] {
            assert!(
                validate_command_pattern(pattern).is_err(),
                "pattern should be rejected: {pattern:?}"
            );
        }
    }

    #[test]
    fn permission_digests_require_canonical_lowercase_hex() {
        assert!(super::validate_digest(&"a".repeat(SHA256_HEX_LEN)).is_ok());
        assert!(super::validate_digest(&"A".repeat(SHA256_HEX_LEN)).is_err());
    }

    #[test]
    fn command_pattern_requires_command_resource_kind() {
        let mut rule = command_pattern_rule(PermissionLifetime::Conversation, "git status *");
        rule.resources[0].kind = PermissionResourceKind::File;

        let message = invalid_message(PermissionRuleRecord::conversation(rule).unwrap_err());

        assert_eq!(
            message,
            "command pattern selector requires a command resource"
        );
    }

    #[test]
    fn command_pattern_cannot_be_an_attribute_selector() {
        let mut rule = rule(PermissionLifetime::Conversation);
        rule.resources[0].attributes.insert(
            "working_directory".into(),
            PermissionResourceSelector::CommandPattern {
                pattern: "git status *".into(),
            },
        );

        let message = invalid_message(PermissionRuleRecord::conversation(rule).unwrap_err());

        assert_eq!(
            message,
            "command pattern selector cannot be used for resource attribute \"working_directory\""
        );
    }

    #[test]
    fn command_pattern_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&state_dir).unwrap();
        let rule = command_pattern_rule(PermissionLifetime::Global, "git status *");

        let inserted = state.insert(None, rule.clone()).unwrap();
        let stored =
            state::get::<Vec<PermissionRuleRecord>>(&state_dir, SCOPE_GLOBAL, PERMISSION_RULES)
                .unwrap()
                .unwrap();
        let stored = serde_json::to_value(&stored[0]).unwrap();
        assert_eq!(
            stored["rule"]["resources"][0]["selector"],
            json!({"match": "command_pattern", "pattern": "git status *"})
        );
        drop(state);

        let restored = PermissionState::open(&state_dir).unwrap();
        assert_eq!(restored.records().len(), 1);
        assert_eq!(restored.records()[0].id, inserted.id);
        assert_eq!(restored.records()[0].rule, rule);
    }

    #[test]
    fn project_and_global_rules_round_trip_and_revoke() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&state_dir).unwrap();

        let project_record = state
            .insert(Some(project.clone()), rule(PermissionLifetime::Project))
            .unwrap();
        let global_record = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        drop(state);

        let mut restored = PermissionState::open(&state_dir).unwrap();
        assert_eq!(restored.records().len(), 2);
        assert_eq!(restored.records()[0].project.as_ref(), Some(&project));
        assert_eq!(restored.records()[1].project, None);
        assert!(restored.revoke(&project_record.id).unwrap());
        assert!(!restored.revoke(&project_record.id).unwrap());
        assert!(restored.records()[0].revoked_at.is_some());
        assert!(restored.records()[1].is_active());
        assert_eq!(restored.records()[1].id, global_record.id);
    }

    #[test]
    fn invalid_state_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut state = PermissionState::open(&state_dir).unwrap();
        let invalid = PermissionRuleRecord {
            id: crate::id::CaudraId::generate().to_string(),
            project: None,
            rule: rule(PermissionLifetime::Project),
            review: None,
            created_at: now_epoch(),
            revoked_at: None,
        };
        state::set(
            &state_dir,
            SCOPE_GLOBAL,
            PERMISSION_RULES,
            &vec![invalid.clone()],
        )
        .unwrap();

        assert!(
            state
                .insert(None, rule(PermissionLifetime::Global))
                .is_err()
        );
        assert_eq!(
            state::get::<Vec<PermissionRuleRecord>>(&state_dir, SCOPE_GLOBAL, PERMISSION_RULES,)
                .unwrap(),
            Some(vec![invalid])
        );
    }

    #[test]
    fn independent_handles_do_not_clobber_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut first = PermissionState::open(&state_dir).unwrap();
        let mut second = PermissionState::open(&state_dir).unwrap();

        first
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        second
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();

        assert_eq!(
            PermissionState::open(&state_dir).unwrap().records().len(),
            2
        );
    }

    #[test]
    fn ephemeral_access_uses_the_persistent_root() {
        let temp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let state_dir = StateDir::split(temp.path().join("volatile"), persistent.path().into());

        PermissionState::open(&state_dir)
            .unwrap()
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();

        assert_eq!(
            PermissionState::open(&persistent).unwrap().records().len(),
            1
        );
        assert!(!state_dir.path().join("sessions.sqlite3").exists());
    }

    #[test]
    fn review_metadata_must_be_redacted() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&state_dir).unwrap();
        state
            .insert_with_review(
                None,
                rule(PermissionLifetime::Global),
                Some(json!({"<field:1>": "<string:6 chars>"})),
            )
            .unwrap();

        assert!(
            state
                .insert_with_review(
                    None,
                    rule(PermissionLifetime::Global),
                    Some(json!({"<field:1>": "secret"})),
                )
                .is_err()
        );
        assert!(
            state
                .insert_with_review(
                    None,
                    rule(PermissionLifetime::Global),
                    Some(json!({"secret": "<string:6 chars>"})),
                )
                .is_err()
        );
    }
}
