use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::MakiId;
use crate::{StateDir, atomic_write_permissions, exclusive_state_lock, now_epoch};

pub const PERMISSION_STATE_FILE: &str = "permission-rules.json";

const PERMISSION_STATE_VERSION: u32 = 2;
const LEGACY_PERMISSION_STATE_VERSION: u32 = 1;
const PERMISSION_STATE_MODE: u32 = 0o600;
const PERMISSION_STATE_LOCK_FILE: &str = "permission-rules.lock";
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
            id: MakiId::generate().to_string(),
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
    #[error("permission state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("permission state is corrupt: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported permission state version {found} (expected {expected})")]
    Version { found: u32, expected: u32 },
    #[error("invalid permission state: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PermissionStateFile {
    version: u32,
    records: Vec<PermissionRuleRecord>,
}

pub struct PermissionState {
    path: PathBuf,
    existed: bool,
    file: PermissionStateFile,
}

impl PermissionState {
    pub fn open(state_dir: &StateDir) -> Result<Self, PermissionStateError> {
        let path = state_dir.path().join(PERMISSION_STATE_FILE);
        match load_file(&path)? {
            Some(file) => Ok(Self {
                path,
                existed: true,
                file,
            }),
            None => Ok(Self {
                path,
                existed: false,
                file: PermissionStateFile {
                    version: PERMISSION_STATE_VERSION,
                    records: Vec::new(),
                },
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn refresh(&mut self) -> Result<(), PermissionStateError> {
        match load_file(&self.path)? {
            Some(file) => {
                self.file = file;
                self.existed = true;
                Ok(())
            }
            None if !self.existed => Ok(()),
            None => Err(PermissionStateError::Invalid(
                "permission state file disappeared".into(),
            )),
        }
    }

    pub fn records(&self) -> &[PermissionRuleRecord] {
        &self.file.records
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
        let lock_path = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(PERMISSION_STATE_LOCK_FILE);
        let _lock =
            exclusive_state_lock(&lock_path, PERMISSION_STATE_MODE).map_err(
                |error| match error {
                    crate::StorageError::Io(error) => PermissionStateError::Io(error),
                    other => PermissionStateError::Invalid(other.to_string()),
                },
            )?;
        self.refresh()?;
        let record = PermissionRuleRecord {
            id: MakiId::generate().to_string(),
            project,
            rule,
            review,
            created_at: now_epoch(),
            revoked_at: None,
        };
        validate_record(&record, true)?;
        let mut next = self.file.clone();
        next.records.push(record.clone());
        self.write(next)?;
        Ok(record)
    }

    pub fn revoke(&mut self, id: &str) -> Result<bool, PermissionStateError> {
        let lock_path = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(PERMISSION_STATE_LOCK_FILE);
        let _lock =
            exclusive_state_lock(&lock_path, PERMISSION_STATE_MODE).map_err(
                |error| match error {
                    crate::StorageError::Io(error) => PermissionStateError::Io(error),
                    other => PermissionStateError::Invalid(other.to_string()),
                },
            )?;
        self.refresh()?;
        let Some(index) = self
            .file
            .records
            .iter()
            .position(|record| record.id == id && record.is_active())
        else {
            return Ok(false);
        };
        let mut next = self.file.clone();
        next.records[index].revoked_at = Some(now_epoch());
        self.write(next)?;
        Ok(true)
    }

    fn write(&mut self, next: PermissionStateFile) -> Result<(), PermissionStateError> {
        validate_file(&next)?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let data = serde_json::to_vec_pretty(&next)?;
        atomic_write_permissions(&self.path, &data, PERMISSION_STATE_MODE).map_err(|error| {
            match error {
                crate::StorageError::Io(error) => PermissionStateError::Io(error),
                crate::StorageError::Json(error) => PermissionStateError::Json(error),
                other => PermissionStateError::Invalid(other.to_string()),
            }
        })?;
        self.file = next;
        self.existed = true;
        Ok(())
    }
}

pub fn validate_conversation_record(
    record: &PermissionRuleRecord,
) -> Result<(), PermissionStateError> {
    validate_record(record, false)
}

fn load_file(path: &Path) -> Result<Option<PermissionStateFile>, PermissionStateError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut file: PermissionStateFile = serde_json::from_slice(&data)?;
    if file.version == LEGACY_PERMISSION_STATE_VERSION {
        file.version = PERMISSION_STATE_VERSION;
    }
    validate_file(&file)?;
    Ok(Some(file))
}

fn validate_file(file: &PermissionStateFile) -> Result<(), PermissionStateError> {
    if file.version != PERMISSION_STATE_VERSION {
        return Err(PermissionStateError::Version {
            found: file.version,
            expected: PERMISSION_STATE_VERSION,
        });
    }
    let mut ids = HashSet::with_capacity(file.records.len());
    for record in &file.records {
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
        .parse::<MakiId>()
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
        validate_selector(&resource.selector)?;
        for selector in resource.attributes.values() {
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

fn validate_selector(selector: &PermissionResourceSelector) -> Result<(), PermissionStateError> {
    match selector {
        PermissionResourceSelector::Digest { digest }
        | PermissionResourceSelector::FilesystemSubtreeDigest { digest }
        | PermissionResourceSelector::UrlSubtreeDigest { digest }
        | PermissionResourceSelector::UrlOriginDigest { digest } => validate_digest(digest),
        PermissionResourceSelector::Any => Ok(()),
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Subtree { .. } => {
            Err(PermissionStateError::Invalid(
                "raw resource values cannot be stored durably".into(),
            ))
        }
    }
}

fn validate_digest(digest: &str) -> Result<(), PermissionStateError> {
    if digest.len() == SHA256_HEX_LEN && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(PermissionStateError::Invalid(
            "invalid SHA-256 digest".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use serde_json::json;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn rule(lifetime: PermissionLifetime) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: "maki".into(),
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
    fn corrupt_state_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let path = state_dir.path().join(PERMISSION_STATE_FILE);
        let corrupt = b"{not valid JSON";
        fs::write(&path, corrupt).unwrap();

        assert!(PermissionState::open(&state_dir).is_err());
        assert_eq!(fs::read(path).unwrap(), corrupt);
    }

    #[test]
    fn version_one_exact_rules_migrate_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let mut state = PermissionState::open(&state_dir).unwrap();
        state
            .insert(Some(project), rule(PermissionLifetime::Project))
            .unwrap();
        drop(state);
        let path = state_dir.path().join(PERMISSION_STATE_FILE);
        let mut stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        stored["version"] = Value::from(LEGACY_PERMISSION_STATE_VERSION);
        fs::write(&path, serde_json::to_vec_pretty(&stored).unwrap()).unwrap();

        let restored = PermissionState::open(&state_dir).unwrap();
        assert_eq!(restored.records().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&state_dir).unwrap();
        state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();

        let mode = fs::metadata(state.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, PERMISSION_STATE_MODE);
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
