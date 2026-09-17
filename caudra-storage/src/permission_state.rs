use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, SessionWorkspaceBinding,
};

use crate::id::CaudraId;
use crate::permission_patterns::PatternDefinition;
use crate::sessions::SessionDatabase;
use crate::state::{SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir, StorageError, now_epoch};
use mutation::{
    PermissionCommitReceipt, PermissionGeneration, PermissionMutation, PermissionMutationError,
    PermissionOwner, PermissionRecordIdentity, PermissionSnapshot, PreparedPermissionMutation,
    prepare_mutation,
};

pub mod mutation;

const PERMISSION_RULES: StateKey = StateKey {
    name: "permission.rules",
    class: StateClass::Persistent,
};
/// Shared between `validate_family` and its tests so a wording change cannot
/// silently pass an assertion.
pub const FAMILY_REQUIRES_RESOURCES: &str =
    "filesystem read family requires at least one resource constraint";
pub const FAMILY_REQUIRES_FILESYSTEM_KIND: &str =
    "filesystem read family requires file or directory resources";
pub const FAMILY_REQUIRES_READ_ACCESS: &str =
    "filesystem read family requires read or search access";
pub const FAMILY_REQUIRES_MCP_SUBJECT: &str = "mcp server family requires an mcp subject";
pub const BROWSE_REQUIRES_NATIVE_SUBJECT: &str =
    "filesystem browse requires a native Workcell names-only contract";
pub const BROWSE_REQUIRES_DIRECTORY_LIST: &str =
    "filesystem browse requires explicit directory list resources";
pub const BROWSE_REQUIRES_BOUNDED_RECURSION: &str =
    "filesystem browse requires a bounded root and pinned direct or recursive enumeration";
pub const BROWSE_RECURSION_ATTRIBUTE: &str = "browse_recursion";
pub const BROWSE_DIRECT: &str = "direct";
pub const BROWSE_RECURSIVE: &str = "recursive";
pub const FILESYSTEM_BROWSE_CONTRACTS: &[&str] = &["file.read.v1", "file.glob.v1"];
pub const RAW_RESOURCE_VALUES_NOT_DURABLE: &str = "raw resource values cannot be stored durably";
pub const COMMAND_PATTERN_MAX_BYTES: usize = 256;
pub const COMMAND_PATTERN_MAX_TOKENS: usize = 8;
const SHA256_HEX_LEN: usize = 64;
const STALE_INVENTORY: &str = "permission inventory changed; create and review a fresh preview";
pub const REVIEW_MAX_STRING_BYTES: usize = 4096;
pub const REVIEW_MAX_RESOURCES: usize = 128;
pub const REVIEW_MAX_ATTRIBUTES: usize = 32;
pub const REVIEW_MAX_INPUT_DEPTH: usize = 8;
pub const REVIEW_MAX_INPUT_NODES: usize = 512;
pub const REVIEW_MAX_INPUT_BYTES: usize = 32_768;
pub const REVIEW_MAX_JSON_BYTES: usize = 131_072;
const INVALID_REVIEW: &str = "permission review exceeds schema bounds";
pub const COMMAND_TEMPLATE_INVALID_CONTEXT: &str =
    "command templates require a native Workcell shell command with pinned execution context";
pub const COMMAND_TEMPLATE_INVALID_ATTRIBUTE: &str =
    "command templates permit only pinned workdir and confined-read attributes";
const WORKCELL_OWNER: &str = "workcell";
const SHELL_EXECUTION_CONTRACT: &str = "shell.execution.v1";
const WORKDIR_ATTRIBUTE: &str = "workdir";
const CONFINED_READ_ATTRIBUTE: &str = "confined_read";
const COMMAND_OBSERVATION_ATTRIBUTE: &str = "command_observation";
const COMMAND_OBSERVATION_BINDING_ATTRIBUTE: &str = "command_observation_binding";
pub const PERMISSION_LABEL_MAX_BYTES: usize = 256;
const INVALID_LABEL: &str = "permission label must be nonempty, bounded, and free of controls";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePermissionIdentity {
    pub authority: AuthorityIdentity,
    pub principal: AuthenticatedPrincipalId,
    pub project: ProjectIdentity,
}

impl RemotePermissionIdentity {
    pub fn from_binding(binding: &SessionWorkspaceBinding) -> Self {
        Self {
            authority: binding.authority().clone(),
            principal: binding.principal().clone(),
            project: binding.project().clone(),
        }
    }

    fn is_consistent(&self) -> bool {
        self.authority.legacy_local_authority_id().is_none()
            && self.principal.authority() == &self.authority
            && self.project.authority() == &self.authority
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
    RemoteWorkcell {
        identity: RemotePermissionIdentity,
        tool: String,
        contract: String,
    },
    RemoteNative {
        identity: RemotePermissionIdentity,
        owner: String,
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
    RemoteWorkcell,
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
    RemoteFile {
        identity: RemotePermissionIdentity,
    },
    RemoteDirectory {
        identity: RemotePermissionIdentity,
    },
    RemoteResource {
        identity: RemotePermissionIdentity,
        resource_kind: String,
    },
    Custom {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionResourceAccess {
    Read,
    List,
    Write,
    Execute,
    Search,
    Connect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionResourceSelector {
    Exact {
        value: String,
    },
    Digest {
        digest: String,
    },
    FilesystemSubtreeDigest {
        digest: String,
    },
    UrlSubtreeDigest {
        digest: String,
    },
    UrlOriginDigest {
        digest: String,
    },
    CommandPattern {
        pattern: String,
    },
    CommandTemplate {
        definition: Box<PatternDefinition>,
    },
    RemoteResource {
        identity: RemotePermissionIdentity,
        scope: Vec<String>,
    },
    RemoteSubtree {
        identity: RemotePermissionIdentity,
        scope: Vec<String>,
    },
    Subtree {
        root: String,
    },
    /// Everything whose value starts with `value`, which is what a configured
    /// scope ending in a bare `*` has always meant. Carries raw text, so like
    /// `Exact` and `Subtree` it is a configured selector and never a stored one.
    Prefix {
        value: String,
    },
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// What a rule says about the requests it matches.
///
/// `Ask` grants no authority; it withholds one. A rule set that would otherwise
/// allow a request still has to prompt when an `Ask` rule matches it, which is
/// how a configured "always confirm this" survives a broad allow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredPermissionEffect {
    Allow,
    Deny,
    Ask,
}

/// Widens a rule from the single tool contract and access mode it was minted
/// from to the family of operations carrying identical authority, so one grant
/// covers reading, listing, and searching a subtree instead of only the tool
/// that happened to ask first.
///
/// A family never crosses a security boundary. `FilesystemRead` is validated to
/// hold only `Read` and `Search` constraints, and the write contracts emit
/// `Write` exclusively, so no grant minted from a read can authorize a mutation.
///
/// Absent on every rule written before families existed, and absent whenever a
/// rule is exact, so an older binary that ignores this field falls back to exact
/// subject matching and merely prompts more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum PermissionCapabilityFamily {
    FilesystemRead,
    FilesystemBrowse,
    /// Widens a rule from the one MCP tool it was minted from to every tool on
    /// the same server. The server is read from the rule's own subject, so the
    /// family stays closed and a rule can never reach a server it was not
    /// written against.
    McpServer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredPermissionRule {
    pub subject: PermissionSubject,
    pub executor: PermissionExecutorKind,
    pub resources: Vec<PermissionResourceConstraint>,
    pub arguments: PermissionArgumentConstraint,
    pub lifetime: PermissionLifetime,
    pub effect: StructuredPermissionEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<PermissionCapabilityFamily>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionReviewSource {
    Approved,
    Recovered,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionReviewResource {
    pub index: usize,
    pub value: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionReview {
    pub tool: String,
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub resources: Vec<PermissionReviewResource>,
    pub source: PermissionReviewSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    pub rule: StructuredPermissionRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<PermissionReview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<PermissionRecordIdentity>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawPermissionSession {
    pub id: CaudraId,
    pub write_version: i64,
    pub cwd: String,
    pub metadata: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawPermissionSnapshot {
    pub persistent: Option<String>,
    pub sessions: Vec<RawPermissionSession>,
}

#[derive(Debug, Default, Serialize)]
pub struct PermissionHistoryScan {
    pub rows: usize,
    pub bytes: usize,
    pub oversized_rows: usize,
    pub truncated: bool,
}

pub fn read_repair_record(
    value: &Value,
    persistent: bool,
) -> Result<PermissionRuleRecord, PermissionStateError> {
    let mut value = value.clone();
    value
        .as_object_mut()
        .ok_or_else(invalid_repair)?
        .remove("review");
    let record = serde_json::from_value(value).map_err(|_| invalid_repair())?;
    validate_record(&record, persistent).map_err(|_| invalid_repair())?;
    Ok(record)
}

pub(crate) fn validate_review_only_change(
    before: &Value,
    after: &Value,
    persistent: bool,
) -> Result<(), PermissionStateError> {
    let before = before.as_array().ok_or_else(invalid_repair)?;
    let after = after.as_array().ok_or_else(invalid_repair)?;
    if before.len() != after.len() {
        return Err(invalid_repair());
    }
    let mut ids = HashSet::new();
    for (before, after) in before.iter().zip(after) {
        let record = read_repair_record(before, persistent)?;
        if !ids.insert(record.id) {
            return Err(invalid_repair());
        }
        let record = serde_json::from_value(after.clone()).map_err(|_| invalid_repair())?;
        validate_record(&record, persistent).map_err(|_| invalid_repair())?;
        let mut before = before.clone();
        let mut after = after.clone();
        before
            .as_object_mut()
            .ok_or_else(invalid_repair)?
            .remove("review");
        after
            .as_object_mut()
            .ok_or_else(invalid_repair)?
            .remove("review");
        if before != after {
            return Err(invalid_repair());
        }
    }
    Ok(())
}

fn invalid_repair() -> PermissionStateError {
    PermissionStateError::Invalid(
        "invalid permission review repair; authority must remain unchanged".into(),
    )
}

impl PermissionRuleRecord {
    pub fn conversation(rule: StructuredPermissionRule) -> Result<Self, PermissionStateError> {
        Self::conversation_with_review(rule, None)
    }

    pub fn conversation_with_review(
        rule: StructuredPermissionRule,
        review: Option<PermissionReview>,
    ) -> Result<Self, PermissionStateError> {
        let record = Self {
            id: CaudraId::generate().to_string(),
            project: None,
            rule,
            review,
            label: None,
            replaces: None,
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
    database: SessionDatabase,
    existed: bool,
    records: Vec<PermissionRuleRecord>,
}

impl PermissionState {
    pub fn open(state_dir: &StateDir) -> Result<Self, PermissionStateError> {
        let database = SessionDatabase::open_state(&state_dir.for_class(PERMISSION_RULES.class))
            .map_err(StorageError::from)?;
        let records = database
            .state_get::<Vec<PermissionRuleRecord>>(SCOPE_GLOBAL, PERMISSION_RULES.name)
            .map_err(StorageError::from)?;
        let existed = records.is_some();
        let records = records.unwrap_or_default();
        validate_records(&records)?;
        Ok(Self {
            database,
            existed,
            records,
        })
    }

    pub fn refresh(&mut self) -> Result<(), PermissionStateError> {
        match self
            .database
            .state_get::<Vec<PermissionRuleRecord>>(SCOPE_GLOBAL, PERMISSION_RULES.name)
            .map_err(StorageError::from)?
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

    pub fn snapshot(&self) -> Result<PermissionSnapshot, PermissionMutationError> {
        let snapshot = self
            .database
            .permission_snapshot(PermissionOwner::Persistent)?;
        if self.existed && !snapshot.revision.row_present {
            return Err(PermissionMutationError::Conflict {
                owner: PermissionOwner::Persistent,
            });
        }
        Ok(snapshot)
    }

    pub fn generation(&self) -> Result<PermissionGeneration, PermissionMutationError> {
        self.database.permission_generation()
    }

    pub fn commit_mutation(
        &mut self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionMutationError> {
        if !prepared.persistent_only() {
            return Err(PermissionMutationError::InvalidOperation);
        }
        let receipt = self.database.commit_permission_mutation(prepared)?;
        self.refresh()?;
        Ok(receipt)
    }

    pub fn mutation_receipt(
        &self,
        operation_id: CaudraId,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionMutationError> {
        self.database.permission_receipt(operation_id)
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
        review: Option<PermissionReview>,
    ) -> Result<PermissionRuleRecord, PermissionStateError> {
        self.insert_many_with_review(vec![(project, rule, review)])?
            .pop()
            .ok_or_else(|| PermissionStateError::Invalid("missing inserted permission".into()))
    }

    pub fn insert_many_with_review(
        &mut self,
        entries: Vec<(
            Option<PathBuf>,
            StructuredPermissionRule,
            Option<PermissionReview>,
        )>,
    ) -> Result<Vec<PermissionRuleRecord>, PermissionStateError> {
        let inserted = entries
            .into_iter()
            .map(|(project, rule, review)| new_record(project, rule, review))
            .collect::<Result<Vec<_>, _>>()?;
        if inserted.is_empty() {
            return Ok(inserted);
        }
        let prepared = prepare_mutation(
            vec![self.snapshot().map_err(mutation_state_error)?],
            PermissionMutation::Create {
                destination: PermissionOwner::Persistent,
                records: inserted.clone().into_boxed_slice(),
            },
        )
        .map_err(mutation_state_error)?;
        self.commit_mutation(&prepared)
            .map_err(mutation_state_error)?;
        Ok(inserted)
    }

    pub fn revoke(&mut self, id: &str) -> Result<bool, PermissionStateError> {
        let snapshot = self.snapshot().map_err(mutation_state_error)?;
        if !snapshot
            .records
            .iter()
            .any(|record| record.id == id && record.is_active())
        {
            self.existed |= snapshot.revision.row_present;
            self.records = snapshot.records;
            return Ok(false);
        }
        let prepared = prepare_mutation(
            vec![snapshot],
            PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: PermissionOwner::Persistent,
                    record_id: id.into(),
                },
            },
        )
        .map_err(mutation_state_error)?;
        self.commit_mutation(&prepared)
            .map_err(mutation_state_error)?;
        Ok(true)
    }
}

fn mutation_state_error(error: PermissionMutationError) -> PermissionStateError {
    match error {
        PermissionMutationError::Session(error) => StorageError::from(error).into(),
        PermissionMutationError::State(error) => error,
        error => PermissionStateError::Invalid(error.to_string()),
    }
}

pub fn read_inventory(
    state_dir: &StateDir,
) -> Result<Vec<PermissionRuleRecord>, PermissionStateError> {
    let database = SessionDatabase::open_read_only(&state_dir.for_class(PERMISSION_RULES.class))
        .map_err(StorageError::from)?;
    let records = database
        .state_get::<Vec<PermissionRuleRecord>>(SCOPE_GLOBAL, PERMISSION_RULES.name)
        .map_err(StorageError::from)?
        .unwrap_or_default();
    validate_records(&records)?;
    Ok(records)
}

pub fn inventory_fingerprint(
    records: &[PermissionRuleRecord],
) -> Result<String, PermissionStateError> {
    validate_records(records)?;
    let encoded = serde_json::to_vec(records).map_err(StorageError::from)?;
    Ok(sha256_hex(&encoded))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn replace_reviewed(
    state_dir: &StateDir,
    expected_fingerprint: &str,
    replacements: Vec<(
        String,
        Option<PathBuf>,
        StructuredPermissionRule,
        Option<PermissionReview>,
    )>,
    recheck: impl FnOnce() -> Result<(), PermissionStateError>,
) -> Result<Vec<PermissionRuleRecord>, PermissionStateError> {
    if replacements.is_empty() {
        return Err(PermissionStateError::Invalid(
            "select at least one replacement".into(),
        ));
    }
    let mut sources = HashSet::new();
    let replacements = replacements
        .into_iter()
        .map(|(source, project, rule, review)| {
            if !sources.insert(source.clone()) {
                return Err(PermissionStateError::Invalid(
                    "duplicate replacement source".into(),
                ));
            }
            let record = new_record(project, rule, review)?;
            Ok((source, record))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut database =
        SessionDatabase::open_permission_admin(&state_dir.for_class(PERMISSION_RULES.class))
            .map_err(StorageError::from)?;
    database
        .state_try_update_checked(
            SCOPE_GLOBAL,
            PERMISSION_RULES.name,
            |records: &mut Vec<PermissionRuleRecord>| -> Result<_, PermissionStateError> {
                if inventory_fingerprint(records)? != expected_fingerprint {
                    return Err(PermissionStateError::Invalid(STALE_INVENTORY.into()));
                }
                for (source, replacement) in &replacements {
                    let original = records
                        .iter_mut()
                        .find(|record| &record.id == source && record.is_active())
                        .ok_or_else(|| {
                            PermissionStateError::Invalid("replacement source is not active".into())
                        })?;
                    if original.rule.effect != replacement.rule.effect {
                        return Err(PermissionStateError::Invalid(
                            "rebinding cannot change a rule's effect".into(),
                        ));
                    }
                    if original.rule.effect == StructuredPermissionEffect::Allow {
                        original.revoked_at = Some(now_epoch().max(original.created_at));
                    }
                    records.push(replacement.clone());
                }
                validate_records(records)?;
                Ok(replacements.into_iter().map(|(_, record)| record).collect())
            },
            recheck,
        )
        .map_err(StorageError::from)?
}

fn new_record(
    project: Option<PathBuf>,
    rule: StructuredPermissionRule,
    review: Option<PermissionReview>,
) -> Result<PermissionRuleRecord, PermissionStateError> {
    let record = PermissionRuleRecord {
        id: CaudraId::generate().to_string(),
        project,
        rule,
        review,
        label: None,
        replaces: None,
        created_at: now_epoch(),
        revoked_at: None,
    };
    validate_record(&record, true)?;
    Ok(record)
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
    if record.label.as_ref().is_some_and(|label| {
        label.trim().is_empty()
            || label.len() > PERMISSION_LABEL_MAX_BYTES
            || label.chars().any(|character| {
                character.is_control()
                    || matches!(character, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            })
    }) {
        return Err(PermissionStateError::Invalid(INVALID_LABEL.into()));
    }
    if let Some(source) = &record.replaces {
        source.record_id.parse::<CaudraId>().map_err(|error| {
            PermissionStateError::Invalid(format!("invalid replacement source ID: {error}"))
        })?;
        if source.record_id == record.id {
            return Err(PermissionStateError::Invalid(
                "a rule cannot replace itself".into(),
            ));
        }
    }
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
    if let Some(family) = record.rule.family {
        validate_family(
            family,
            &record.rule.subject,
            &record.rule.executor,
            &record.rule.resources,
        )?;
    }
    validate_subject(&record.rule.subject)?;
    validate_command_templates(&record.rule)?;
    for resource in &record.rule.resources {
        validate_resource_selector(&resource.kind, &resource.selector)?;
        for (attribute, selector) in &resource.attributes {
            if matches!(
                attribute.as_str(),
                COMMAND_OBSERVATION_ATTRIBUTE | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
            ) {
                return Err(PermissionStateError::Invalid(
                    COMMAND_TEMPLATE_INVALID_ATTRIBUTE.into(),
                ));
            }
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

pub fn validate_command_templates(
    rule: &StructuredPermissionRule,
) -> Result<(), PermissionStateError> {
    for resource in &rule.resources {
        let PermissionResourceSelector::CommandTemplate { definition } = &resource.selector else {
            continue;
        };
        if rule.executor != PermissionExecutorKind::Native
            || !matches!(&rule.subject, PermissionSubject::Native { owner, contract }
                if owner == WORKCELL_OWNER && contract == SHELL_EXECUTION_CONTRACT)
            || rule.family.is_some()
            || resource.kind != PermissionResourceKind::Command
            || resource.access != Some(PermissionResourceAccess::Execute)
            || resource.protected != Some(false)
            || !PathBuf::from(&definition.context.effective_workdir).is_absolute()
        {
            return Err(PermissionStateError::Invalid(
                COMMAND_TEMPLATE_INVALID_CONTEXT.into(),
            ));
        }
        definition
            .validate()
            .map_err(|error| PermissionStateError::Invalid(error.to_string()))?;
        let workdir_digest = Sha256::digest(
            serde_json::to_vec(&definition.context.effective_workdir)
                .map_err(|error| PermissionStateError::Invalid(error.to_string()))?,
        );
        let workdir_digest: String = workdir_digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if !matches!(resource.attributes.get(WORKDIR_ATTRIBUTE),
            Some(PermissionResourceSelector::Digest { digest }) if digest == &workdir_digest)
            || resource.attributes.iter().any(|(name, selector)| {
                !matches!(name.as_str(), WORKDIR_ATTRIBUTE | CONFINED_READ_ATTRIBUTE)
                    || !matches!(selector, PermissionResourceSelector::Digest { .. })
            })
        {
            return Err(PermissionStateError::Invalid(
                COMMAND_TEMPLATE_INVALID_ATTRIBUTE.into(),
            ));
        }
    }
    Ok(())
}

fn validate_subject(subject: &PermissionSubject) -> Result<(), PermissionStateError> {
    match subject {
        PermissionSubject::RemoteWorkcell { identity, .. }
        | PermissionSubject::RemoteNative { identity, .. }
            if !identity.is_consistent() =>
        {
            Err(PermissionStateError::Invalid(
                "remote permission subject identity is inconsistent".into(),
            ))
        }
        _ => Ok(()),
    }
}

fn validate_review(review: &PermissionReview) -> Result<(), PermissionStateError> {
    let bounded = |text: &str| text.len() <= REVIEW_MAX_STRING_BYTES;
    let mut indices = HashSet::new();
    let mut nodes = REVIEW_MAX_INPUT_NODES;
    if review.tool.is_empty()
        || review.authority.is_empty()
        || !bounded(&review.tool)
        || !bounded(&review.authority)
        || review.resources.len() > REVIEW_MAX_RESOURCES
        || review.resources.iter().any(|resource| {
            !indices.insert(resource.index)
                || resource
                    .value
                    .as_deref()
                    .is_some_and(|value| !bounded(value))
                || resource.attributes.len() > REVIEW_MAX_ATTRIBUTES
                || resource.attributes.iter().any(|(key, value)| {
                    matches!(
                        key.as_str(),
                        COMMAND_OBSERVATION_ATTRIBUTE | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
                    ) || !bounded(key)
                        || !bounded(value)
                })
        })
        || review.input.as_ref().is_some_and(|input| {
            !bounded_review_input(input, 0, &mut nodes)
                || serde_json::to_vec(input)
                    .map_or(true, |bytes| bytes.len() > REVIEW_MAX_INPUT_BYTES)
        })
        || serde_json::to_vec(review).map_or(true, |bytes| bytes.len() > REVIEW_MAX_JSON_BYTES)
    {
        return Err(PermissionStateError::Invalid(INVALID_REVIEW.into()));
    }
    Ok(())
}

fn bounded_review_input(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    if depth > REVIEW_MAX_INPUT_DEPTH || *nodes == 0 {
        return false;
    }
    *nodes -= 1;
    match value {
        Value::String(value) => value.len() <= REVIEW_MAX_STRING_BYTES,
        Value::Array(values) => values
            .iter()
            .all(|value| bounded_review_input(value, depth + 1, nodes)),
        Value::Object(values) => values.iter().all(|(key, value)| {
            key.len() <= REVIEW_MAX_STRING_BYTES && bounded_review_input(value, depth + 1, nodes)
        }),
        _ => true,
    }
}

/// The invariant that keeps a widened rule from becoming an escalation. Every
/// constraint must name a filesystem kind and an explicit read-shaped access, so
/// a stored `FilesystemRead` rule cannot match a `Write` resource no matter which
/// contract later presents it. An absent access would mean "any", so it is rejected.
fn validate_family(
    family: PermissionCapabilityFamily,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    resources: &[PermissionResourceConstraint],
) -> Result<(), PermissionStateError> {
    match family {
        PermissionCapabilityFamily::FilesystemBrowse => {
            validate_filesystem_browse(subject, executor, resources)
        }
        PermissionCapabilityFamily::McpServer => {
            // The server name is read off the subject, so a non-MCP subject
            // would widen the rule to nothing it could name.
            if !matches!(subject, PermissionSubject::Mcp { .. }) {
                return Err(PermissionStateError::Invalid(
                    FAMILY_REQUIRES_MCP_SUBJECT.into(),
                ));
            }
            Ok(())
        }
        PermissionCapabilityFamily::FilesystemRead => {
            if resources.is_empty() {
                return Err(PermissionStateError::Invalid(
                    FAMILY_REQUIRES_RESOURCES.into(),
                ));
            }
            for resource in resources {
                if !matches!(
                    resource.kind,
                    PermissionResourceKind::File | PermissionResourceKind::Directory
                ) {
                    return Err(PermissionStateError::Invalid(
                        FAMILY_REQUIRES_FILESYSTEM_KIND.into(),
                    ));
                }
                if !matches!(
                    resource.access,
                    Some(PermissionResourceAccess::Read) | Some(PermissionResourceAccess::Search)
                ) {
                    return Err(PermissionStateError::Invalid(
                        FAMILY_REQUIRES_READ_ACCESS.into(),
                    ));
                }
            }
            Ok(())
        }
    }
}

pub fn filesystem_browse_recursion(selector: &PermissionResourceSelector) -> Option<&'static str> {
    [BROWSE_DIRECT, BROWSE_RECURSIVE]
        .into_iter()
        .find(|value| match selector {
            PermissionResourceSelector::Exact { value: expected } => expected == value,
            PermissionResourceSelector::Digest { digest } => {
                *digest
                    == Sha256::digest(format!("\"{value}\""))
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
            }
            _ => false,
        })
}

pub fn validate_filesystem_browse(
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    resources: &[PermissionResourceConstraint],
) -> Result<(), PermissionStateError> {
    if *executor != PermissionExecutorKind::Native
        || !matches!(subject, PermissionSubject::Native { owner, contract }
            if owner == WORKCELL_OWNER && FILESYSTEM_BROWSE_CONTRACTS.contains(&contract.as_str()))
    {
        return Err(PermissionStateError::Invalid(
            BROWSE_REQUIRES_NATIVE_SUBJECT.into(),
        ));
    }
    if resources.is_empty()
        || resources.iter().any(|resource| {
            resource.kind != PermissionResourceKind::Directory
                || resource.access != Some(PermissionResourceAccess::List)
        })
    {
        return Err(PermissionStateError::Invalid(
            BROWSE_REQUIRES_DIRECTORY_LIST.into(),
        ));
    }
    for resource in resources {
        let recursion = resource
            .attributes
            .get(BROWSE_RECURSION_ATTRIBUTE)
            .and_then(filesystem_browse_recursion);
        let bounded = match &resource.selector {
            PermissionResourceSelector::Exact { .. }
            | PermissionResourceSelector::Digest { .. } => recursion.is_some(),
            PermissionResourceSelector::Subtree { .. }
            | PermissionResourceSelector::FilesystemSubtreeDigest { .. } => {
                recursion == Some(BROWSE_RECURSIVE)
            }
            _ => false,
        };
        if !bounded {
            return Err(PermissionStateError::Invalid(
                BROWSE_REQUIRES_BOUNDED_RECURSION.into(),
            ));
        }
    }
    Ok(())
}

fn validate_resource_selector(
    kind: &PermissionResourceKind,
    selector: &PermissionResourceSelector,
) -> Result<(), PermissionStateError> {
    if let PermissionResourceSelector::CommandTemplate { definition } = selector {
        if *kind != PermissionResourceKind::Command {
            return Err(PermissionStateError::Invalid(
                COMMAND_TEMPLATE_INVALID_CONTEXT.into(),
            ));
        }
        return definition
            .validate()
            .map_err(|error| PermissionStateError::Invalid(error.to_string()));
    }
    if matches!(
        kind,
        PermissionResourceKind::RemoteFile { identity }
            | PermissionResourceKind::RemoteDirectory { identity }
            | PermissionResourceKind::RemoteResource { identity, .. }
            if !identity.is_consistent()
    ) {
        return Err(PermissionStateError::Invalid(
            "remote resource kind identity is inconsistent".into(),
        ));
    }
    if matches!(
        selector,
        PermissionResourceSelector::RemoteResource { .. }
            | PermissionResourceSelector::RemoteSubtree { .. }
    ) && !matches!(
        kind,
        PermissionResourceKind::RemoteFile { .. }
            | PermissionResourceKind::RemoteDirectory { .. }
            | PermissionResourceKind::RemoteResource { .. }
    ) {
        return Err(PermissionStateError::Invalid(
            "remote resource selector requires a remote resource kind".into(),
        ));
    }
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
        PermissionResourceSelector::RemoteResource { identity, scope }
        | PermissionResourceSelector::RemoteSubtree { identity, scope }
            if identity.is_consistent()
                && !scope.is_empty()
                && scope
                    .iter()
                    .all(|value| !value.is_empty() && !value.chars().any(char::is_control))
                && scope.iter().collect::<HashSet<_>>().len() == scope.len() =>
        {
            Ok(())
        }
        PermissionResourceSelector::RemoteResource { .. }
        | PermissionResourceSelector::RemoteSubtree { .. } => Err(PermissionStateError::Invalid(
            "remote resource selector identity is invalid".into(),
        )),
        PermissionResourceSelector::CommandPattern { .. }
        | PermissionResourceSelector::CommandTemplate { .. } => Err(PermissionStateError::Invalid(
            "command pattern selector is only valid as a primary command resource selector".into(),
        )),
        PermissionResourceSelector::Exact { .. }
        | PermissionResourceSelector::Subtree { .. }
        | PermissionResourceSelector::Prefix { .. } => Err(PermissionStateError::Invalid(
            RAW_RESOURCE_VALUES_NOT_DURABLE.into(),
        )),
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
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(unix)]
    use std::process::Command;

    use serde_json::json;

    use test_case::test_case;

    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
    };

    use super::{
        COMMAND_PATTERN_MAX_BYTES, FAMILY_REQUIRES_FILESYSTEM_KIND, FAMILY_REQUIRES_MCP_SUBJECT,
        FAMILY_REQUIRES_READ_ACCESS, FAMILY_REQUIRES_RESOURCES, INVALID_REVIEW, PERMISSION_RULES,
        PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionExecutorKind,
        PermissionLifetime, PermissionResourceAccess, PermissionResourceConstraint,
        PermissionResourceKind, PermissionResourceSelector, PermissionReview,
        PermissionReviewResource, PermissionReviewSource, PermissionRuleRecord, PermissionState,
        PermissionStateError, PermissionSubject, RAW_RESOURCE_VALUES_NOT_DURABLE,
        REVIEW_MAX_ATTRIBUTES, REVIEW_MAX_INPUT_BYTES, REVIEW_MAX_INPUT_DEPTH,
        REVIEW_MAX_INPUT_NODES, REVIEW_MAX_JSON_BYTES, REVIEW_MAX_RESOURCES,
        REVIEW_MAX_STRING_BYTES, RemotePermissionIdentity, SHA256_HEX_LEN, STALE_INVENTORY,
        StructuredPermissionEffect, StructuredPermissionRule, inventory_fingerprint,
        read_inventory, replace_reviewed, validate_command_pattern, validate_conversation_record,
    };
    use crate::id::CaudraId;
    use crate::sessions::{SESSIONS_DB_FILE, SessionLease};
    use crate::state::{self, SCOPE_GLOBAL};
    use crate::{StateDir, now_epoch};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const INJECTED_FAILURE: &str = "injected permission write failure";
    #[cfg(unix)]
    const RESOLUTION_CHILD: &str = "CAUDRA_TEST_PERMISSION_RESOLUTION_CHILD";

    #[cfg(unix)]
    #[test]
    fn permission_resolution_does_not_create_directories() {
        if let Some(root) = env::var_os(RESOLUTION_CHILD) {
            let dir = StateDir::resolve_without_create().unwrap();
            assert!(dir.path().starts_with(&root));
            assert!(!dir.path().exists());
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("uncreated");
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "permission_state::tests::permission_resolution_does_not_create_directories",
            ])
            .env(RESOLUTION_CHILD, &root)
            .env("XDG_STATE_HOME", &root)
            .env("XDG_DATA_HOME", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test_case("permissive"; "mode")]
    #[test_case("directory"; "file_type")]
    #[test_case("symlink"; "symlink")]
    fn permission_admin_refuses_insecure_database_without_repair(kind: &str) {
        const PERMISSIVE_MODE: u32 = 0o644;
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let fingerprint = inventory_fingerprint(state.records()).unwrap();
        drop(state);
        let path = dir.path().join(SESSIONS_DB_FILE);
        let before = fs::read(&path).unwrap();
        let backup = temp.path().join("original");
        match kind {
            "permissive" => {
                fs::set_permissions(&path, fs::Permissions::from_mode(PERMISSIVE_MODE)).unwrap()
            }
            "directory" => {
                fs::rename(&path, &backup).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                fs::rename(&path, &backup).unwrap();
                symlink(&backup, &path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            replace_reviewed(
                &dir,
                &fingerprint,
                vec![(source.id, None, source.rule, None)],
                || Ok(())
            )
            .is_err()
        );
        if kind == "permissive" {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                PERMISSIVE_MODE
            );
            assert_eq!(fs::read(&path).unwrap(), before);
        } else {
            assert_eq!(fs::read(&backup).unwrap(), before);
            assert_eq!(
                fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                kind == "symlink"
            );
        }
    }

    #[test_case(StructuredPermissionEffect::Deny; "deny")]
    #[test_case(StructuredPermissionEffect::Ask; "ask")]
    fn restrictive_rebind_retains_source_policy_with_omitted_allows(
        effect: StructuredPermissionEffect,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let mut restrictive = rule(PermissionLifetime::Global);
        restrictive.effect = effect;
        let source = state.insert(None, restrictive).unwrap();
        let original = state.records().to_vec();
        let fingerprint = inventory_fingerprint(&original).unwrap();
        drop(state);
        let inserted = replace_reviewed(
            &dir,
            &fingerprint,
            vec![(source.id, None, source.rule, None)],
            || Ok(()),
        )
        .unwrap();
        let records = read_inventory(&dir).unwrap();
        assert_eq!(&records[..original.len()], original);
        assert_eq!(records.last(), inserted.first());
        assert!(records.iter().all(PermissionRuleRecord::is_active));
    }

    #[test]
    fn pre_commit_refusal_rolls_back_rebind() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let original = state.records().to_vec();
        let fingerprint = inventory_fingerprint(&original).unwrap();
        drop(state);
        let result = replace_reviewed(
            &dir,
            &fingerprint,
            vec![(source.id, None, source.rule, None)],
            || Err(PermissionStateError::Invalid(INJECTED_FAILURE.into())),
        );
        assert_eq!(invalid_message(result.unwrap_err()), INJECTED_FAILURE);
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    #[test]
    fn reviewed_replacement_database_failure_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let original = state.records().to_vec();
        let fingerprint = inventory_fingerprint(&original).unwrap();
        drop(state);
        let connection = Connection::open(temp.path().join(SESSIONS_DB_FILE)).unwrap();
        connection.execute_batch(&format!("CREATE TRIGGER fail_permissions BEFORE UPDATE ON state WHEN NEW.key = 'permission.rules' BEGIN SELECT RAISE(ABORT, '{INJECTED_FAILURE}'); END;")).unwrap();
        drop(connection);
        let error = replace_reviewed(
            &dir,
            &fingerprint,
            vec![(source.id, None, source.rule, None)],
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains(INJECTED_FAILURE));
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    #[test_case(false; "invalid_second_rule")]
    #[test_case(true; "database_write_failure")]
    fn composed_insert_is_atomic(fail_write: bool) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let original = state.records().to_vec();
        if fail_write {
            let connection = Connection::open(temp.path().join(SESSIONS_DB_FILE)).unwrap();
            connection.execute_batch(&format!("CREATE TRIGGER fail_permissions BEFORE UPDATE ON state WHEN NEW.key = 'permission.rules' BEGIN SELECT RAISE(ABORT, '{INJECTED_FAILURE}'); END;")).unwrap();
        }
        let second = if fail_write {
            PermissionLifetime::Global
        } else {
            PermissionLifetime::Project
        };
        let result = state.insert_many_with_review(vec![
            (None, rule(PermissionLifetime::Global), None),
            (None, rule(second), None),
        ]);
        assert!(result.is_err());
        if fail_write {
            assert!(result.unwrap_err().to_string().contains(INJECTED_FAILURE));
        }
        assert_eq!(state.records(), original);
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    #[test]
    fn composed_insert_refreshes_only_after_commit() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let inserted = state
            .insert_many_with_review(vec![
                (None, rule(PermissionLifetime::Global), None),
                (None, rule(PermissionLifetime::Global), None),
            ])
            .unwrap();
        assert_eq!(inserted.len(), 2);
        assert_eq!(state.records(), inserted);
        assert_eq!(read_inventory(&dir).unwrap(), inserted);
    }

    #[test_case(false; "missing_second_source")]
    #[test_case(true; "stale_snapshot")]
    fn reviewed_replacement_rolls_back(stale: bool) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let fingerprint = inventory_fingerprint(state.records()).unwrap();
        if stale {
            state
                .insert(None, rule(PermissionLifetime::Global))
                .unwrap();
        }
        let original = state.records().to_vec();
        drop(state);
        let result = replace_reviewed(
            &dir,
            &fingerprint,
            vec![
                (source.id, None, rule(PermissionLifetime::Global), None),
                (
                    CaudraId::generate().to_string(),
                    None,
                    rule(PermissionLifetime::Global),
                    None,
                ),
            ],
            || Ok(()),
        );
        assert!(result.is_err());
        if stale {
            assert_eq!(invalid_message(result.unwrap_err()), STALE_INVENTORY);
        }
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    #[test]
    fn reviewed_replacement_retains_original_and_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let fingerprint = inventory_fingerprint(state.records()).unwrap();
        drop(state);
        let inserted = replace_reviewed(
            &dir,
            &fingerprint,
            vec![(source.id.clone(), None, source.rule.clone(), Some(review()))],
            || Ok(()),
        )
        .unwrap();
        let records = read_inventory(&dir).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].rule, source.rule);
        assert!(!records[0].is_active());
        assert_eq!(records[1], inserted[0]);
        assert_eq!(records[1].review, Some(review()));
    }

    #[test]
    fn reviewed_replacement_refuses_live_session() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        let mut state = PermissionState::open(&dir).unwrap();
        let source = state
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        let original = state.records().to_vec();
        let fingerprint = inventory_fingerprint(&original).unwrap();
        drop(state);
        let _lease = SessionLease::acquire(&dir, CaudraId::generate()).unwrap();
        assert!(
            replace_reviewed(
                &dir,
                &fingerprint,
                vec![(source.id, None, source.rule, None)],
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(read_inventory(&dir).unwrap(), original);
    }

    #[test]
    fn read_only_inventory_never_initializes_storage() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().join("absent"));
        assert!(read_inventory(&dir).is_err());
        assert!(!dir.path().exists());
    }

    fn remote_identity() -> RemotePermissionIdentity {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("https://authority.example").unwrap(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        RemotePermissionIdentity {
            principal: AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            project: ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap()),
            authority,
        }
    }

    #[test]
    fn permission_identities_written_before_remote_workcell_still_deserialize() {
        let subject: PermissionSubject = serde_json::from_value(json!({
            "kind":"native",
            "owner":"workcell",
            "contract":"file.read.v1"
        }))
        .expect("legacy native subject");
        let executor: PermissionExecutorKind =
            serde_json::from_value(json!("native")).expect("legacy native executor");
        assert_eq!(
            subject,
            PermissionSubject::Native {
                owner: "workcell".into(),
                contract: "file.read.v1".into(),
            }
        );
        assert_eq!(executor, PermissionExecutorKind::Native);
    }

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
            family: None,
        }
    }

    #[test]
    fn remote_workcell_rule_round_trips_with_opaque_resource_authority() {
        let identity = remote_identity();
        let rule = StructuredPermissionRule {
            subject: PermissionSubject::RemoteWorkcell {
                identity: identity.clone(),
                tool: "file_read".into(),
                contract: "file.read.v1@v1/v1".into(),
            },
            executor: PermissionExecutorKind::RemoteWorkcell,
            resources: vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::RemoteFile {
                    identity: identity.clone(),
                },
                selector: PermissionResourceSelector::RemoteResource {
                    identity,
                    scope: vec!["root".into(), "opaque-file".into()],
                },
                access: Some(PermissionResourceAccess::Read),
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        };
        let encoded = serde_json::to_string(&rule).expect("remote rule serializes");
        let decoded: StructuredPermissionRule =
            serde_json::from_str(&encoded).expect("remote rule deserializes");
        assert_eq!(decoded, rule);

        let mut unknown = serde_json::to_value(&rule).unwrap();
        unknown["subject"]["server_generation"] = serde_json::json!("instance");
        assert!(serde_json::from_value::<StructuredPermissionRule>(unknown).is_err());

        let legacy_remote = json!({
            "subject": {
                "kind": "remote_workcell",
                "server": "server",
                "workspace": "workspace",
                "principal": "principal",
                "root_project": "project",
                "tool": "file_read",
                "contract": "file.read.v1"
            },
            "executor": "remote_workcell",
            "resources": [],
            "arguments": {"constraint": "unconstrained"},
            "lifetime": "conversation",
            "effect": "allow"
        });
        assert!(serde_json::from_value::<StructuredPermissionRule>(legacy_remote).is_err());
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
            label: None,
            replaces: None,
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
    fn refresh_sees_writes_from_an_independent_handle() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut writer = PermissionState::open(&state_dir).unwrap();
        let mut reader = PermissionState::open(&state_dir).unwrap();

        let record = writer
            .insert(None, rule(PermissionLifetime::Global))
            .unwrap();
        assert!(reader.records().is_empty());
        reader.refresh().unwrap();
        assert_eq!(reader.records().len(), 1);
        assert!(reader.records()[0].is_active());

        assert!(writer.revoke(&record.id).unwrap());
        reader.refresh().unwrap();
        assert!(!reader.records()[0].is_active());
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
        assert!(!state_dir.path().join("caudra.sqlite").exists());
    }

    fn review() -> PermissionReview {
        PermissionReview {
            tool: "file_read".into(),
            authority: "Bound tool; exact input".into(),
            input: Some(json!({"filePath": "/project/src/lib.rs", "offset": 12, "limit": 30})),
            resources: vec![],
            source: PermissionReviewSource::Recovered,
        }
    }

    #[test_case("string"; "oversize_string")]
    #[test_case("depth"; "oversize_depth")]
    #[test_case("nodes"; "oversize_nodes")]
    #[test_case("input_bytes"; "oversize_input_bytes")]
    #[test_case("json_bytes"; "oversize_json_bytes")]
    #[test_case("resources"; "oversize_resources")]
    #[test_case("attributes"; "oversize_attributes")]
    #[test_case("duplicate"; "duplicate_resource_index")]
    fn review_metadata_enforces_schema_bounds(case: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut state = PermissionState::open(&state_dir).unwrap();
        state
            .insert_with_review(None, rule(PermissionLifetime::Global), Some(review()))
            .unwrap();

        let mut invalid = review();
        let resource = PermissionReviewResource {
            index: 0,
            value: None,
            attributes: BTreeMap::new(),
        };
        match case {
            "string" => invalid.tool = "x".repeat(REVIEW_MAX_STRING_BYTES + 1),
            "depth" => {
                let mut value = json!(null);
                for _ in 0..=REVIEW_MAX_INPUT_DEPTH {
                    value = json!([value]);
                }
                invalid.input = Some(value);
            }
            "nodes" => invalid.input = Some(json!(vec![true; REVIEW_MAX_INPUT_NODES])),
            "input_bytes" => {
                invalid.input = Some(json!(vec![
                    "x".repeat(REVIEW_MAX_STRING_BYTES);
                    REVIEW_MAX_INPUT_BYTES / REVIEW_MAX_STRING_BYTES
                        + 1
                ]))
            }
            "json_bytes" => {
                invalid.resources = (0..=REVIEW_MAX_JSON_BYTES / REVIEW_MAX_STRING_BYTES)
                    .map(|index| PermissionReviewResource {
                        index,
                        value: Some("x".repeat(REVIEW_MAX_STRING_BYTES)),
                        attributes: BTreeMap::new(),
                    })
                    .collect()
            }
            "resources" => {
                invalid.resources = (0..=REVIEW_MAX_RESOURCES)
                    .map(|index| PermissionReviewResource {
                        index,
                        ..resource.clone()
                    })
                    .collect()
            }
            "attributes" => {
                invalid.resources = vec![PermissionReviewResource {
                    attributes: (0..=REVIEW_MAX_ATTRIBUTES)
                        .map(|index| (index.to_string(), String::new()))
                        .collect(),
                    ..resource
                }]
            }
            "duplicate" => invalid.resources = vec![resource.clone(), resource],
            _ => unreachable!(),
        }
        let error = state
            .insert_with_review(None, rule(PermissionLifetime::Global), Some(invalid))
            .unwrap_err();
        assert_eq!(invalid_message(error), INVALID_REVIEW);
        assert_eq!(
            PermissionState::open(&state_dir).unwrap().records(),
            state.records()
        );
    }

    #[test_case(json!({"<field:1>": "<string:6 chars>"}); "anonymous_shape_refused")]
    #[test_case(json!("<rebound-from:id>"); "anonymous_string_refused")]
    #[test_case(json!({"tool": "bash", "authority": "exact", "resources": [], "source": "approved", "unknown": true}); "unknown_field_refused")]
    fn review_deserialization_requires_typed_schema(value: serde_json::Value) {
        assert!(serde_json::from_value::<PermissionReview>(value).is_err());
    }

    fn family_rule(
        kind: PermissionResourceKind,
        access: Option<PermissionResourceAccess>,
    ) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: "workcell".into(),
                contract: "file.read.v1".into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: vec![PermissionResourceConstraint {
                kind,
                selector: PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: DIGEST.into(),
                },
                access,
                protected: Some(false),
                attributes: BTreeMap::new(),
            }],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: Some(PermissionCapabilityFamily::FilesystemRead),
        }
    }

    fn family_error(rule: StructuredPermissionRule) -> String {
        match PermissionRuleRecord::conversation(rule) {
            Err(PermissionStateError::Invalid(message)) => message,
            other => panic!("expected an invalid-record error, got {other:?}"),
        }
    }

    #[test_case(
        PermissionResourceKind::File,
        Some(PermissionResourceAccess::Write),
        FAMILY_REQUIRES_READ_ACCESS;
        "write access cannot ride on a read family"
    )]
    #[test_case(
        PermissionResourceKind::File,
        Some(PermissionResourceAccess::Execute),
        FAMILY_REQUIRES_READ_ACCESS;
        "execute access cannot ride on a read family"
    )]
    #[test_case(
        PermissionResourceKind::File,
        None,
        FAMILY_REQUIRES_READ_ACCESS;
        "an unconstrained access would reach write"
    )]
    #[test_case(
        PermissionResourceKind::Command,
        Some(PermissionResourceAccess::Read),
        FAMILY_REQUIRES_FILESYSTEM_KIND;
        "a command resource is not a filesystem read"
    )]
    fn a_stored_read_family_cannot_widen_past_reading(
        kind: PermissionResourceKind,
        access: Option<PermissionResourceAccess>,
        expected: &str,
    ) {
        assert_eq!(family_error(family_rule(kind, access)), expected);
    }

    #[test]
    fn a_read_family_rule_needs_a_resource_to_widen() {
        let mut rule = family_rule(
            PermissionResourceKind::File,
            Some(PermissionResourceAccess::Read),
        );
        rule.resources.clear();

        assert_eq!(family_error(rule), FAMILY_REQUIRES_RESOURCES);
    }

    /// A selector holding raw text cannot be stored: a record outlives the run
    /// that wrote it, and raw text is not a stable name for a resource.
    #[test_case(PermissionResourceSelector::Exact { value: "/project/notes.md".into() } ; "an exact value")]
    #[test_case(PermissionResourceSelector::Subtree { root: "/project".into() } ; "a subtree root")]
    #[test_case(PermissionResourceSelector::Prefix { value: "/project/gen".into() } ; "a prefix")]
    fn a_raw_selector_cannot_be_stored_durably(selector: PermissionResourceSelector) {
        let mut rule = family_rule(
            PermissionResourceKind::File,
            Some(PermissionResourceAccess::Read),
        );
        rule.family = None;
        rule.resources[0].selector = selector;

        assert_eq!(family_error(rule), RAW_RESOURCE_VALUES_NOT_DURABLE);
    }

    /// The server is read off the rule's own subject, so a subject that names no
    /// server would widen the rule to a set it cannot compute.
    #[test]
    fn an_mcp_server_family_is_refused_without_an_mcp_subject() {
        let mut rule = family_rule(
            PermissionResourceKind::File,
            Some(PermissionResourceAccess::Read),
        );
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        assert_eq!(family_error(rule), FAMILY_REQUIRES_MCP_SUBJECT);
    }

    /// The server family widens the subject, not the operation, so it carries
    /// none of the read family's resource restrictions.
    #[test]
    fn an_mcp_server_family_stores_and_reloads() {
        let mut rule = family_rule(
            PermissionResourceKind::Command,
            Some(PermissionResourceAccess::Execute),
        );
        rule.subject = PermissionSubject::Mcp {
            server: "deepwiki".into(),
            authority: "deepwiki".into(),
            tool: "search".into(),
            contract: "mcp.tool.v1".into(),
        };
        rule.executor = PermissionExecutorKind::Mcp;
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        let record = PermissionRuleRecord::conversation(rule).expect("record is valid");
        let encoded = serde_json::to_string(&record).expect("record serializes");
        let decoded: PermissionRuleRecord =
            serde_json::from_str(&encoded).expect("record deserializes");

        assert_eq!(decoded.rule.family, record.rule.family);
        assert!(validate_conversation_record(&decoded).is_ok());
    }

    #[test_case(PermissionResourceAccess::Read; "read joins the family")]
    #[test_case(PermissionResourceAccess::Search; "search joins the family")]
    fn a_read_family_rule_stores_and_reloads(access: PermissionResourceAccess) {
        let rule = family_rule(PermissionResourceKind::Directory, Some(access));

        let record = PermissionRuleRecord::conversation(rule).expect("record is valid");
        let encoded = serde_json::to_string(&record).expect("record serializes");
        let decoded: PermissionRuleRecord =
            serde_json::from_str(&encoded).expect("record deserializes");

        assert_eq!(decoded.rule.family, record.rule.family);
        assert!(validate_conversation_record(&decoded).is_ok());
    }

    #[test]
    fn an_exact_rule_stores_no_family_key_and_older_records_load_as_exact() {
        let record = PermissionRuleRecord::conversation(rule(PermissionLifetime::Conversation))
            .expect("record is valid");

        let encoded = serde_json::to_value(&record).expect("record serializes");

        assert!(
            encoded["rule"].get("family").is_none(),
            "an exact rule must not grow a family key: {encoded}"
        );
        let decoded: PermissionRuleRecord =
            serde_json::from_value(encoded).expect("a record without a family still loads");
        assert_eq!(decoded.rule.family, None);
    }
}

#[cfg(test)]
mod browse_tests {
    use super::{
        BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE,
        BROWSE_REQUIRES_BOUNDED_RECURSION, BROWSE_REQUIRES_DIRECTORY_LIST,
        BROWSE_REQUIRES_NATIVE_SUBJECT, PermissionArgumentConstraint, PermissionCapabilityFamily,
        PermissionExecutorKind, PermissionLifetime, PermissionResourceAccess,
        PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
        PermissionRuleRecord, PermissionStateError, PermissionSubject, StructuredPermissionEffect,
        StructuredPermissionRule, validate_conversation_record,
    };
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use test_case::test_case;

    const ROOT_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn browse_rule(recursion: &str) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: "workcell".into(),
                contract: "file.glob.v1".into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Directory,
                selector: PermissionResourceSelector::Digest {
                    digest: ROOT_DIGEST.into(),
                },
                access: Some(PermissionResourceAccess::List),
                protected: Some(false),
                attributes: BTreeMap::from([(
                    BROWSE_RECURSION_ATTRIBUTE.into(),
                    PermissionResourceSelector::Digest {
                        digest: Sha256::digest(format!("\"{recursion}\""))
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                    },
                )]),
            }],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: Some(PermissionCapabilityFamily::FilesystemBrowse),
        }
    }

    #[test_case(BROWSE_DIRECT; "direct_exact")]
    #[test_case(BROWSE_RECURSIVE; "recursive_subtree")]
    fn browse_family_roundtrips(recursion: &str) {
        let mut rule = browse_rule(recursion);
        if recursion == BROWSE_RECURSIVE {
            rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: ROOT_DIGEST.into(),
            };
        }
        let record = PermissionRuleRecord::conversation(rule).unwrap();
        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: PermissionRuleRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.rule, record.rule);
        assert!(validate_conversation_record(&decoded).is_ok());
    }

    #[test_case("read", BROWSE_REQUIRES_DIRECTORY_LIST; "no_content_access")]
    #[test_case("search", BROWSE_REQUIRES_DIRECTORY_LIST; "no_generic_search")]
    #[test_case("write", BROWSE_REQUIRES_DIRECTORY_LIST; "no_write_access")]
    #[test_case("any_access", BROWSE_REQUIRES_DIRECTORY_LIST; "no_missing_access")]
    #[test_case("file", BROWSE_REQUIRES_DIRECTORY_LIST; "directory_only")]
    #[test_case("empty", BROWSE_REQUIRES_DIRECTORY_LIST; "nonempty_resources")]
    #[test_case("contract", BROWSE_REQUIRES_NATIVE_SUBJECT; "no_grep_contract")]
    #[test_case("owner", BROWSE_REQUIRES_NATIVE_SUBJECT; "trusted_owner_only")]
    #[test_case("executor", BROWSE_REQUIRES_NATIVE_SUBJECT; "native_executor_only")]
    #[test_case("no_recursion", BROWSE_REQUIRES_BOUNDED_RECURSION; "recursion_required")]
    #[test_case("unknown_recursion", BROWSE_REQUIRES_BOUNDED_RECURSION; "canonical_recursion_only")]
    #[test_case("direct_subtree", BROWSE_REQUIRES_BOUNDED_RECURSION; "direct_is_exact")]
    #[test_case("any_root", BROWSE_REQUIRES_BOUNDED_RECURSION; "bounded_root_required")]
    fn invalid_browse_families_are_rejected(change: &str, expected: &str) {
        let mut rule = browse_rule(BROWSE_DIRECT);
        match change {
            "read" => rule.resources[0].access = Some(PermissionResourceAccess::Read),
            "search" => rule.resources[0].access = Some(PermissionResourceAccess::Search),
            "write" => rule.resources[0].access = Some(PermissionResourceAccess::Write),
            "any_access" => rule.resources[0].access = None,
            "file" => rule.resources[0].kind = PermissionResourceKind::File,
            "empty" => rule.resources.clear(),
            "contract" => {
                rule.subject = PermissionSubject::Native {
                    owner: "workcell".into(),
                    contract: "file.grep.v1".into(),
                }
            }
            "owner" => {
                rule.subject = PermissionSubject::Native {
                    owner: "other".into(),
                    contract: "file.glob.v1".into(),
                }
            }
            "executor" => rule.executor = PermissionExecutorKind::Mcp,
            "no_recursion" => rule.resources[0].attributes.clear(),
            "unknown_recursion" => {
                rule.resources[0].attributes = browse_rule("unknown").resources.remove(0).attributes
            }
            "direct_subtree" => {
                rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: ROOT_DIGEST.into(),
                }
            }
            _ => rule.resources[0].selector = PermissionResourceSelector::Any,
        }
        let Err(PermissionStateError::Invalid(message)) = PermissionRuleRecord::conversation(rule)
        else {
            panic!("invalid browse family was accepted");
        };
        assert_eq!(message, expected);
    }
}

#[cfg(test)]
mod command_template_tests {
    use super::{
        COMMAND_TEMPLATE_INVALID_ATTRIBUTE, COMMAND_TEMPLATE_INVALID_CONTEXT,
        CONFINED_READ_ATTRIBUTE, PermissionArgumentConstraint, PermissionCapabilityFamily,
        PermissionExecutorKind, PermissionLifetime, PermissionResourceAccess,
        PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
        PermissionRuleRecord, PermissionState, PermissionSubject, SHELL_EXECUTION_CONTRACT,
        StructuredPermissionEffect, StructuredPermissionRule, WORKCELL_OWNER, WORKDIR_ATTRIBUTE,
    };
    use crate::StateDir;
    use crate::permission_patterns::{
        ArgumentRole, PATTERN_SCHEMA_VERSION, PatternContext, PatternDefinition, PatternToken,
        SlotCombinations,
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use test_case::test_case;

    const WORKDIR: &str = "/project";

    fn template_rule() -> StructuredPermissionRule {
        let definition = PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: "Check".into(),
            context: PatternContext {
                tool_identity: "native/shell/v1".into(),
                executable_identity: "cargo-v1".into(),
                effective_workdir: WORKDIR.into(),
                path_binding: "local/project".into(),
                analysis_version: "static/v1".into(),
            },
            argv: vec![PatternToken::Exact {
                value: "cargo".into(),
                role: ArgumentRole::Executable,
            }],
            slots: Vec::new(),
            combinations: SlotCombinations::Independent,
        };
        let digest = Sha256::digest(serde_json::to_vec(WORKDIR).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: SHELL_EXECUTION_CONTRACT.into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: vec![PermissionResourceConstraint {
                kind: PermissionResourceKind::Command,
                selector: PermissionResourceSelector::CommandTemplate {
                    definition: Box::new(definition),
                },
                access: Some(PermissionResourceAccess::Execute),
                protected: Some(false),
                attributes: BTreeMap::from([(
                    WORKDIR_ATTRIBUTE.into(),
                    PermissionResourceSelector::Digest { digest },
                )]),
            }],
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        }
    }

    #[test_case("owner"; "owner")]
    #[test_case("contract"; "contract")]
    #[test_case("executor"; "executor")]
    #[test_case("resource_kind"; "resource_kind")]
    #[test_case("access"; "access")]
    #[test_case("protection"; "protection")]
    fn command_template_requires_native_shell_execution(change: &str) {
        let mut rule = template_rule();
        match change {
            "owner" => {
                rule.subject = PermissionSubject::Native {
                    owner: "plugin".into(),
                    contract: SHELL_EXECUTION_CONTRACT.into(),
                }
            }
            "contract" => {
                rule.subject = PermissionSubject::Native {
                    owner: WORKCELL_OWNER.into(),
                    contract: "another-contract".into(),
                }
            }
            "executor" => rule.executor = PermissionExecutorKind::Lua,
            "resource_kind" => rule.resources[0].kind = PermissionResourceKind::File,
            "access" => rule.resources[0].access = None,
            "protection" => rule.resources[0].protected = Some(true),
            _ => unreachable!(),
        }
        let error = PermissionRuleRecord::conversation(rule).unwrap_err();
        assert!(error.to_string().contains(COMMAND_TEMPLATE_INVALID_CONTEXT));
    }

    #[test_case("missing_workdir"; "missing_workdir")]
    #[test_case("widened_workdir"; "widened_workdir")]
    #[test_case("different_workdir"; "different_workdir")]
    #[test_case("observation"; "observation")]
    #[test_case("unknown"; "unknown")]
    #[test_case("confined_any"; "confined_any")]
    fn command_template_rejects_unpinned_or_hidden_attributes(change: &str) {
        let mut rule = template_rule();
        let attributes = &mut rule.resources[0].attributes;
        match change {
            "missing_workdir" => {
                attributes.remove(WORKDIR_ATTRIBUTE);
            }
            "widened_workdir" => {
                attributes.insert(WORKDIR_ATTRIBUTE.into(), PermissionResourceSelector::Any);
            }
            "different_workdir" => {
                attributes.insert(
                    WORKDIR_ATTRIBUTE.into(),
                    PermissionResourceSelector::Digest {
                        digest: "0".repeat(64),
                    },
                );
            }
            "observation" => {
                attributes.insert(
                    super::COMMAND_OBSERVATION_ATTRIBUTE.into(),
                    PermissionResourceSelector::Any,
                );
            }
            "unknown" => {
                attributes.insert("roles".into(), PermissionResourceSelector::Any);
            }
            "confined_any" => {
                attributes.insert(
                    CONFINED_READ_ATTRIBUTE.into(),
                    PermissionResourceSelector::Any,
                );
            }
            _ => unreachable!(),
        }
        let error = PermissionRuleRecord::conversation(rule).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(COMMAND_TEMPLATE_INVALID_ATTRIBUTE)
        );
    }

    #[test_case("constraint"; "constraint")]
    #[test_case("selector"; "selector")]
    #[test_case("definition"; "definition")]
    #[test_case("rule"; "rule")]
    fn command_template_serde_rejects_unknown_authority_fields(location: &str) {
        let mut wire = serde_json::to_value(template_rule()).unwrap();
        let location = match location {
            "constraint" => &mut wire["resources"][0],
            "selector" => &mut wire["resources"][0]["selector"],
            "definition" => &mut wire["resources"][0]["selector"]["definition"],
            "rule" => &mut wire,
            _ => unreachable!(),
        };
        location["role"] = json!("data");
        assert!(serde_json::from_value::<StructuredPermissionRule>(wire).is_err());
    }

    #[test]
    fn command_template_definition_roundtrips_and_invalid_batch_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut state = PermissionState::open(&dir).unwrap();
        let mut valid = template_rule();
        valid.lifetime = PermissionLifetime::Global;
        let mut invalid = valid.clone();
        let PermissionResourceSelector::CommandTemplate { definition } =
            &mut invalid.resources[0].selector
        else {
            unreachable!()
        };
        definition.version += 1;
        assert!(
            state
                .insert_many_with_review(vec![(None, valid.clone(), None), (None, invalid, None)])
                .is_err()
        );
        assert!(state.records().is_empty());
        let record = state.insert(None, valid).unwrap();
        drop(state);
        assert_eq!(PermissionState::open(&dir).unwrap().records(), &[record]);
    }

    #[test]
    fn command_template_cannot_be_an_attribute_or_capability_family() {
        let mut attribute = template_rule();
        let selector = attribute.resources[0].selector.clone();
        attribute.resources[0].selector = PermissionResourceSelector::Any;
        attribute.resources[0]
            .attributes
            .insert(WORKDIR_ATTRIBUTE.into(), selector);
        assert!(PermissionRuleRecord::conversation(attribute).is_err());
        let mut family = template_rule();
        family.family = Some(PermissionCapabilityFamily::FilesystemRead);
        assert!(PermissionRuleRecord::conversation(family).is_err());
    }
}
